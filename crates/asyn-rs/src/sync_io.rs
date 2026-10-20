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
use crate::request::{RequestOp, RequestResult};
use crate::services::PortServices;
use crate::trace::{TraceManager, TraceMask};
use crate::user::AsynUser;
use epics_libcom_rs::SharedArray;
use epics_libcom_rs::cfmt::CFloat;

/// Synchronous I/O handle backed by a [`PortHandle`] (actor model).
///
/// All operations submit requests to the actor and block until completion.
pub struct SyncIOHandle {
    link: Arc<Link>,
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

/// [`SyncIOHandle::print`] on a user connected to `port`.
#[track_caller]
fn print_on(
    trace: &TraceManager,
    port: &str,
    addr: i32,
    reason: i32,
    mask: TraceMask,
    args: std::fmt::Arguments<'_>,
) {
    trace.print(Some(port), (addr >= 0).then_some(addr), reason, mask, args);
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
            link: Arc::new(Link::Port(handle)),
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
                link: Arc::new(link),
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
        match &*self.link {
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
        match &*self.link {
            Link::Port(handle) | Link::NoInterface(handle, _) => Some(handle),
            Link::NoPort => None,
        }
    }

    /// C `asynPrint(pasynUser, mask, ...)` on this user: the port's rung for a
    /// connected user, and the global trace for one whose connect found no
    /// port (`findTracePvt`, asynManager.c:548-553). `args` is the line
    /// without its newline. The SOURCE prefix names the caller, as C's macro
    /// does (asynDriver.h:296-306).
    #[track_caller]
    pub fn print(&self, reason: i32, mask: TraceMask, args: std::fmt::Arguments<'_>) {
        match self.device() {
            Some(handle) => print_on(
                handle.trace(),
                handle.port_name(),
                self.addr,
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
        self.user_with(reason, self.timeout)
    }

    fn user_with(&self, reason: usize, timeout: Duration) -> AsynUser {
        let user = AsynUser::new(reason)
            .with_addr(self.addr)
            .with_timeout(timeout);
        match self.device() {
            Some(handle) => user.locking_port(handle),
            None => user,
        }
    }

    /// This user with C SyncIO's per-call `timeout` argument
    /// (asynOctetSyncIO.c:187, :213, :245, asynInt32SyncIO.c:139): every call
    /// through the copy passes it, and `self` keeps its own.
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            link: Arc::clone(&self.link),
            addr: self.addr,
            timeout,
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

    /// `op` under queueLockPort, `traced` printing C's TRACEIO_DEVICE line on
    /// its success before the port is released (asynOctetSyncIO.c:194-200).
    fn transfer(
        &self,
        user: AsynUser,
        op: RequestOp,
        traced: impl FnOnce(&RequestResult),
    ) -> AsynResult<RequestResult> {
        self.port()?.submit_blocking_held(op, user, |ran| {
            if let Ok(result) = ran {
                traced(result);
            }
        })
    }

    /// C `asynPrint(pasynUser, ASYN_TRACEIO_DEVICE, ...)` on this user.
    #[track_caller]
    fn device_line(&self, reason: usize, args: std::fmt::Arguments<'_>) {
        self.print(reason as i32, TraceMask::IO_DEVICE, args);
    }

    /// C `asynPrintIO(pasynUser, ASYN_TRACEIO_DEVICE, data, len, label)`.
    #[track_caller]
    fn device_io(&self, reason: usize, data: &[u8], label: &str) {
        if let Some(handle) = self.device() {
            let at = std::panic::Location::caller();
            handle.trace().output_device_io(
                handle.port_name(),
                (self.addr >= 0).then_some(self.addr),
                reason as i32,
                TraceMask::IO_DEVICE,
                data,
                label,
                at.file(),
                at.line(),
            );
        }
    }

    pub fn read_int32(&self, reason: usize) -> AsynResult<i32> {
        let result = self.transfer(self.user(reason), RequestOp::Int32Read, |r| {
            if let Some(v) = r.int_val {
                self.device_line(reason, format_args!("asynInt32SyncIO read: {v}\n"));
            }
        })?;
        result
            .int_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int32 read returned no value".into(),
            })
    }

    pub fn write_int32(&self, reason: usize, value: i32) -> AsynResult<()> {
        let op = RequestOp::Int32Write { value };
        self.transfer(self.user(reason), op, |_| {
            self.device_line(reason, format_args!("asynInt32SyncIO wrote: {value}\n"));
        })?;
        Ok(())
    }

    pub fn read_float64(&self, reason: usize) -> AsynResult<f64> {
        let result = self.transfer(self.user(reason), RequestOp::Float64Read, |r| {
            if let Some(v) = r.float_val {
                self.device_line(
                    reason,
                    format_args!("asynFloat64SyncIO read: {:e}\n", CFloat(v)),
                );
            }
        })?;
        result
            .float_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "float64 read returned no value".into(),
            })
    }

    pub fn write_float64(&self, reason: usize, value: f64) -> AsynResult<()> {
        let op = RequestOp::Float64Write { value };
        self.transfer(self.user(reason), op, |_| {
            self.device_line(
                reason,
                format_args!("asynFloat64SyncIO wrote: {:e}\n", CFloat(value)),
            );
        })?;
        Ok(())
    }

    pub fn read_octet(&self, reason: usize, buf_size: usize) -> crate::request::OctetRead {
        self.octet_read(reason, crate::request::OctetReadOp::Read { buf_size }, None)
    }

    /// `op`, whose write half (a writeRead's) sent `written`.
    fn octet_read(
        &self,
        reason: usize,
        op: crate::request::OctetReadOp,
        written: Option<&[u8]>,
    ) -> crate::request::OctetRead {
        let port = match self.port() {
            Ok(port) => port,
            Err(e) => return crate::request::OctetRead::failed(e),
        };
        let ran = port.submit_blocking_held(op.into_op(), self.user(reason), |ran| {
            let Ok(RequestResult {
                octet: Some(read),
                nbytes,
                ..
            }) = ran
            else {
                return;
            };
            let read_ran = match read.status() {
                Ok(()) => true,
                Err(e) => e.octet_step() == Some(crate::error::OctetStep::Read),
            };
            if let (true, Some(written)) = (read_ran, written) {
                let out = &written[..(*nbytes).min(written.len())];
                self.device_io(reason, out, "asynOctetSyncIO wrote:\n");
            }
            if read.status().is_ok() {
                self.device_io(reason, read.data(), "asynOctetSyncIO read:\n");
            }
        });
        crate::request::octet_outcome(ran)
    }

    /// Returns the number of bytes transferred (C `asynOctet::write`'s
    /// `*nbytesTransferred`).
    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<usize> {
        let op = RequestOp::OctetWrite {
            data: data.to_vec(),
        };
        let result = self.transfer(self.user(reason), op, |_| {
            self.device_io(reason, data, "asynOctetSyncIO wrote:\n");
        })?;
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
            Some(data),
        )
    }

    /// C `pasynOctetSyncIO->flush` (asynOctetSyncIO.c:278-299), which passes
    /// 1 s whatever the user's timeout.
    pub fn flush(&self, reason: usize) -> AsynResult<()> {
        let user = self.user_with(reason, Duration::from_secs(1));
        self.transfer(user, RequestOp::Flush, |_| {
            self.device_line(reason, format_args!("asynOctetSyncIO flush\n"));
        })?;
        Ok(())
    }

    /// C `pasynOctetSyncIO->setInputEos` (asynOctetSyncIO.c:300-320).
    pub fn set_input_eos(&self, reason: usize, eos: &[u8]) -> AsynResult<()> {
        let op = RequestOp::SetInputEos { eos: eos.to_vec() };
        self.eos_call(reason, op, "setInputEos").map(drop)
    }

    /// C `pasynOctetSyncIO->getInputEos` (asynOctetSyncIO.c:322-342), whose
    /// trace line also says `setInputEos`.
    pub fn get_input_eos(&self, reason: usize) -> AsynResult<Vec<u8>> {
        self.eos_call(reason, RequestOp::GetInputEos, "setInputEos")
            .map(|r| r.data.unwrap_or_default())
    }

    /// C `pasynOctetSyncIO->setOutputEos` (asynOctetSyncIO.c:344-364).
    pub fn set_output_eos(&self, reason: usize, eos: &[u8]) -> AsynResult<()> {
        let op = RequestOp::SetOutputEos { eos: eos.to_vec() };
        self.eos_call(reason, op, "setOutputEos").map(drop)
    }

    /// C `pasynOctetSyncIO->getOutputEos` (asynOctetSyncIO.c:366-386), whose
    /// trace line also says `setOutputEos`.
    pub fn get_output_eos(&self, reason: usize) -> AsynResult<Vec<u8>> {
        self.eos_call(reason, RequestOp::GetOutputEos, "setOutputEos")
            .map(|r| r.data.unwrap_or_default())
    }

    /// The EOS calls run under `lockPort`, not `queueLockPort`: no queue, no
    /// connect gate, so a disabled or disconnected port takes them and only a
    /// shut-down one refuses (asynManager.c:1778-1806).
    fn eos_call(
        &self,
        reason: usize,
        op: RequestOp,
        name: &'static str,
    ) -> AsynResult<RequestResult> {
        let handle = match &*self.link {
            Link::NoPort => return Err(refused("asynManager::lockPort not connected\n".into())),
            _ => self.port()?,
        };
        let (addr, timeout) = (self.addr, self.timeout);
        let user = move || AsynUser::new(reason).with_addr(addr).with_timeout(timeout);
        let trace = handle.trace().clone();
        let port = handle.port_name().to_owned();
        handle.lock_port_blocking(user(), move |calls| {
            let result = calls.call(&op, user())?;
            let eoslen = match &op {
                RequestOp::SetInputEos { eos } | RequestOp::SetOutputEos { eos } => eos.len(),
                _ => result.nbytes,
            };
            print_on(
                &trace,
                &port,
                addr,
                reason as i32,
                TraceMask::IO_DEVICE,
                format_args!("asynOctetSyncIO {name} eoslen {eoslen}\n"),
            );
            Ok(result)
        })?
    }

    pub fn read_uint32_digital(&self, reason: usize, mask: u32) -> AsynResult<u32> {
        let op = RequestOp::UInt32DigitalRead { mask };
        let result = self.transfer(self.user(reason), op, |r| {
            if let Some(v) = r.uint_val {
                self.device_line(
                    reason,
                    format_args!("asynUInt32DigitalSyncIO read: {v:#x}\n"),
                );
            }
        })?;
        result
            .uint_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "uint32 read returned no value".into(),
            })
    }

    pub fn write_uint32_digital(&self, reason: usize, value: u32, mask: u32) -> AsynResult<()> {
        let op = RequestOp::UInt32DigitalWrite { value, mask };
        self.transfer(self.user(reason), op, |_| {
            self.device_line(
                reason,
                format_args!("asynUInt32DigitalSyncIO wrote: {value:#x}\n"),
            );
        })?;
        Ok(())
    }

    pub fn read_int64(&self, reason: usize) -> AsynResult<i64> {
        let result = self.transfer(self.user(reason), RequestOp::Int64Read, |r| {
            if let Some(v) = r.int64_val {
                self.device_line(reason, format_args!("asynInt64SyncIO read: {v}\n"));
            }
        })?;
        result
            .int64_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int64 read returned no value".into(),
            })
    }

    pub fn write_int64(&self, reason: usize, value: i64) -> AsynResult<()> {
        let op = RequestOp::Int64Write { value };
        self.transfer(self.user(reason), op, |_| {
            self.device_line(reason, format_args!("asynInt64SyncIO wrote: {value}\n"));
        })?;
        Ok(())
    }

    /// `asynInt32ArraySyncIO->read`: reads into `buf` in place and returns
    /// `nIn`.
    pub fn read_int32_array(&self, reason: usize, buf: &mut [i32]) -> AsynResult<usize> {
        let op = RequestOp::Int32ArrayRead {
            buf: SharedArray::from(&*buf),
            max_elements: buf.len(),
        };
        let first = buf.first().copied();
        let result = self.transfer(self.user(reason), op, |r| {
            let read = r.int32_array.as_ref().and_then(|a| a.as_slice().first());
            if let Some(v) = read.copied().or(first) {
                self.device_line(reason, format_args!("asynInt32ArraySyncIO read: {v}\n"));
            }
        })?;
        let data = result.int32_array.unwrap_or_default();
        buf[..data.len()].copy_from_slice(data.as_slice());
        Ok(data.len())
    }

    /// `asynInt32ArraySyncIO->write`.
    pub fn write_int32_array(&self, reason: usize, data: &[i32]) -> AsynResult<()> {
        let op = RequestOp::Int32ArrayWrite {
            data: data.to_vec(),
        };
        self.transfer(self.user(reason), op, |_| {
            if let Some(v) = data.first() {
                self.device_line(reason, format_args!("asynInt32ArraySyncIO wrote: {v}\n"));
            }
        })?;
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
        let Link::Port(handle) = &*self.link else {
            return Ok(0);
        };
        if !handle.has_interface(crate::interfaces::InterfaceType::DrvUser) {
            return Ok(0);
        }
        // C's `create` runs on the user `connectDevice` put at this addr
        // (:121, :149), so the driver resolves and traces that addr; no record
        // interface is behind the bind.
        handle
            .drv_user_create_blocking(&crate::port::DrvUserRequest::new(drv_info, self.addr))
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

    /// C SyncIO takes its timeout on every call, and `flush` its own 1 s
    /// (asynOctetSyncIO.c:187, :213, :245, :285): the driver sees that call's
    /// timeout, on the caller's thread under queueLockPort.
    #[test]
    fn a_per_call_timeout_reaches_the_driver_on_the_caller() {
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<(&'static str, Option<Duration>, Option<String>)>>>;
        struct Timed {
            base: PortDriverBase,
            seen: Seen,
        }
        impl Timed {
            fn record(&self, what: &'static str, user: &AsynUser) {
                let thread = std::thread::current().name().map(str::to_owned);
                self.seen.lock().unwrap().push((what, user.timeout, thread));
            }
        }
        impl PortDriver for Timed {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
                self.record("flush", user);
                Ok(())
            }
            fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.record("write", user);
                Ok(data.len())
            }
        }

        let seen = Seen::default();
        let drv = Timed {
            base: PortDriverBase::new(
                "sync_timed",
                1,
                PortFlags {
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            seen: Arc::clone(&seen),
        };
        let (rt, _jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let sio = SyncIOHandle::from_handle(rt.port_handle().clone(), 0, Duration::from_secs(3));
        sio.with_timeout(Duration::from_secs(7))
            .write_octet(0, b"x")
            .unwrap();
        sio.flush(0).unwrap();
        sio.write_octet(0, b"y").unwrap();

        let caller = std::thread::current().name().map(str::to_owned);
        let secs = |s| Some(Duration::from_secs(s));
        assert_eq!(
            *seen.lock().unwrap(),
            [
                ("write", secs(7), caller.clone()),
                ("flush", secs(1), caller.clone()),
                ("write", secs(3), caller),
            ]
        );
    }

    /// `port` with only its TRACEIO_DEVICE lines traced, and a user on it.
    fn device_traced(
        port: impl PortDriver,
        name: &str,
        dir: &std::path::Path,
    ) -> (SyncIOHandle, PortRuntimeHandle, std::path::PathBuf) {
        let (rt, path) = traced_port(port, name, dir);
        rt.port_handle()
            .trace()
            .set_trace_mask(Some(name), TraceMask::IO_DEVICE);
        let sio = SyncIOHandle::from_handle(rt.port_handle().clone(), 0, Duration::from_secs(1));
        (sio, rt, path)
    }

    /// Each SyncIO transfer prints its C TRACEIO_DEVICE line once it succeeds
    /// (asynOctetSyncIO.c:194-196, :220-222, :258-268, :290-291,
    /// asynInt32SyncIO.c:146-169, asynFloat64SyncIO.c:139-163,
    /// asynUInt32DigitalSyncIO.c:166-192, asynInt64SyncIO.c), and a refused
    /// one prints none.
    #[test]
    fn each_transfer_prints_its_c_traceio_device_line() {
        let dir = tempfile::tempdir().expect("fixture root");
        let mut port = TestPort::named("sync_lines");
        port.base.flags.destructible = true;
        let (sio, _rt, path) = device_traced(port, "sync_lines", dir.path());

        sio.write_octet(2, b"hi\r").unwrap();
        sio.read_octet(2, 32).into_parts().1.unwrap();
        sio.write_read(2, b"ab", 32).into_parts().1.unwrap();
        sio.flush(2).unwrap();
        sio.write_int32(0, -7).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), -7);
        sio.write_float64(1, 2.5).unwrap();
        assert_eq!(sio.read_float64(1).unwrap(), 2.5);
        sio.write_uint32_digital(3, 0xFF, 0x0F).unwrap();
        assert_eq!(sio.read_uint32_digital(3, 0xFF).unwrap(), 0x0F);
        sio.write_int64(6, -9).unwrap();
        assert_eq!(sio.read_int64(6).unwrap(), -9);
        assert!(sio.read_int32(99).is_err());

        let expected = [
            "[sync_lines,-1,2] asynOctetSyncIO wrote:\nhi\\r\n",
            // asynPortDriver's readOctet counts the NUL (strlen + 1).
            "[sync_lines,-1,2] asynOctetSyncIO read:\nhi\\r\\0\n",
            "[sync_lines,-1,2] asynOctetSyncIO wrote:\nab\n",
            "[sync_lines,-1,2] asynOctetSyncIO read:\nab\\0\n",
            "[sync_lines,-1,2] asynOctetSyncIO flush\n",
            "[sync_lines,-1,0] asynInt32SyncIO wrote: -7\n",
            "[sync_lines,-1,0] asynInt32SyncIO read: -7\n",
            "[sync_lines,-1,1] asynFloat64SyncIO wrote: 2.500000e+00\n",
            "[sync_lines,-1,1] asynFloat64SyncIO read: 2.500000e+00\n",
            "[sync_lines,-1,3] asynUInt32DigitalSyncIO wrote: 0xff\n",
            "[sync_lines,-1,3] asynUInt32DigitalSyncIO read: 0xf\n",
            "[sync_lines,-1,6] asynInt64SyncIO wrote: -9\n",
            "[sync_lines,-1,6] asynInt64SyncIO read: -9\n",
        ]
        .concat();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    /// An int32 array port: a write stores the array, a read returns it.
    struct ArrayPort {
        base: PortDriverBase,
        stored: Vec<i32>,
    }

    impl PortDriver for ArrayPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn write_int32_array(&mut self, _user: &AsynUser, data: &[i32]) -> AsynResult<()> {
            self.stored = data.to_vec();
            Ok(())
        }
        fn read_int32_array(&mut self, _user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
            let n = self.stored.len().min(buf.len());
            buf[..n].copy_from_slice(&self.stored[..n]);
            Ok(n)
        }
    }

    /// The array transfers print the first element, as
    /// asynInt32ArraySyncIO.c:138-142 and :161-164 print `*pvalue`; an empty
    /// read leaves the caller's first element in place, so that is the one
    /// printed.
    #[test]
    fn an_int32_array_transfer_prints_its_first_element() {
        let dir = tempfile::tempdir().expect("fixture root");
        let base = PortDriverBase::new(
            "sync_arrays",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let port = ArrayPort {
            base,
            stored: Vec::new(),
        };
        let (sio, _rt, path) = device_traced(port, "sync_arrays", dir.path());

        let mut empty = [5, 6];
        assert_eq!(sio.read_int32_array(4, &mut empty).unwrap(), 0);
        assert_eq!(empty, [5, 6]);
        sio.write_int32_array(4, &[-3, 8, 9]).unwrap();
        let mut buf = [0; 2];
        assert_eq!(sio.read_int32_array(4, &mut buf).unwrap(), 2);
        assert_eq!(buf, [-3, 8]);

        let expected = [
            "[sync_arrays,-1,4] asynInt32ArraySyncIO read: 5\n",
            "[sync_arrays,-1,4] asynInt32ArraySyncIO wrote: -3\n",
            "[sync_arrays,-1,4] asynInt32ArraySyncIO read: -3\n",
        ]
        .concat();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    /// A writeRead whose read fails has printed its wrote line and not its
    /// read line (asynOctetSyncIO.c:258-268, `goto bad`).
    #[test]
    fn a_write_read_failing_its_read_prints_only_the_write() {
        struct ReadFails(PortDriverBase);
        impl PortDriver for ReadFails {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                crate::interpose::OctetReadResult::failed(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "no reply".into(),
                })
            }
        }
        let dir = tempfile::tempdir().expect("fixture root");
        let mut port = TestPort::named("sync_wr_fail");
        port.base.flags.destructible = true;
        let (sio, _rt, path) = device_traced(ReadFails(port.base), "sync_wr_fail", dir.path());

        let status = sio.write_read(0, b"Q", 8).into_parts().1;
        assert_eq!(status.unwrap_err().status(), AsynStatus::Timeout);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[sync_wr_fail,-1,0] asynOctetSyncIO wrote:\nQ\n"
        );
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

    /// asynOctetSyncIO.c:121, :149: `create` runs on the syncIO user at its
    /// addr, so asynPortDriver's `getAddress` refuses an addr past the port's
    /// range (asynPortDriver.cpp:3569) and the bind's lines name the addr.
    #[test]
    fn drv_user_create_resolves_at_the_sync_io_addr() {
        use crate::trace::{TraceFile, TraceInfoMask};

        let mut base = PortDriverBase::new(
            "sync_drv_addr",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("V", ParamType::Int32).unwrap();
        let (rt, _jh) = create_port_runtime_unclaimed(TestPort { base }, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let trace = rt.port_handle().trace();
        trace.set_trace_mask(Some("sync_drv_addr"), TraceMask::FLOW | TraceMask::ERROR);
        trace.set_trace_info_mask(Some("sync_drv_addr"), TraceInfoMask::PORT);
        let dir = tempfile::tempdir().expect("fixture root");
        let path = dir.path().join("drv.log");
        trace.set_trace_file(
            Some("sync_drv_addr"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(
                std::fs::File::create(&path).unwrap(),
            ))),
        );
        let at = |addr| {
            SyncIOHandle::from_handle(rt.port_handle().clone(), addr, Duration::from_secs(1))
        };

        assert_eq!(at(1).drv_user_create("V").unwrap(), 0);
        assert!(at(5).drv_user_create("V").is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[sync_drv_addr,1,0] asynPortDriver:drvUserCreate: drvInfo=V, index=0\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "[sync_drv_addr,5,0] asynPortDriver:getAddress: sync_drv_addr invalid \
                 address=5, must be in range 0 to 1\n"
            ),
            "{text}"
        );
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
        // The EOS calls take `lockPort`, in its words (asynManager.c:1783-1787).
        let lock_refused = "asynManager::lockPort not connected\n";
        assert_eq!(msg(sio.set_input_eos(0, b"\n").unwrap_err()), lock_refused);
        assert_eq!(msg(sio.get_input_eos(0).unwrap_err()), lock_refused);
        assert_eq!(msg(sio.set_output_eos(0, b"\n").unwrap_err()), lock_refused);
        assert_eq!(msg(sio.get_output_eos(0).unwrap_err()), lock_refused);

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

    /// C `asynPrint` on a syncIO user prints its caller's `__FILE__`/`__LINE__`
    /// (asynDriver.h:296-306), not `[:0]`.
    #[test]
    fn a_print_names_its_caller() {
        use crate::trace::{TraceFile, TraceInfoMask};

        let (handle, _jh) = create_port_runtime_unclaimed(
            TestPort::named("sync_print_source"),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));
        let trace = handle.port_handle().trace();
        trace.set_trace_info_mask(Some("sync_print_source"), TraceInfoMask::SOURCE);
        let dir = tempfile::tempdir().expect("fixture root");
        let path = dir.path().join("sync_print_source.log");
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("sync_print_source"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );

        let line = line!() + 1;
        sio.print(0, TraceMask::ERROR, format_args!("connected line\n"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("[sync_io.rs:{line}] connected line\n"));
    }

    /// A port carrying C's `asynInterposeEos`, its trace in `dir`.
    fn eos_port(name: &str, dir: &std::path::Path) -> (PortRuntimeHandle, std::path::PathBuf) {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let mut port = TestPort::named(name);
        port.base.flags.destructible = true;
        port.base
            .install_octet_interpose(Box::new(EosInterpose::new(EosConfig::default())));
        traced_port(port, name, dir)
    }

    /// `port` running with its IO_DEVICE and FLOW trace in `dir`.
    fn traced_port(
        port: impl PortDriver,
        name: &str,
        dir: &std::path::Path,
    ) -> (PortRuntimeHandle, std::path::PathBuf) {
        use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask};

        let (handle, _jh) = create_port_runtime_unclaimed(port, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let trace = handle.port_handle().trace();
        trace.set_trace_mask(Some(name), TraceMask::IO_DEVICE | TraceMask::FLOW);
        trace.set_trace_info_mask(Some(name), TraceInfoMask::PORT);
        trace.set_trace_io_mask(Some(name), TraceIoMask::ESCAPE);
        let path = dir.join("eos.log");
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some(name),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        (handle, path)
    }

    /// asynOctetSyncIO.c:300-386: the EOS calls take `lockPort`, which has no
    /// queue and no enable gate, so a disabled port takes them while a queued
    /// transfer is refused. Each success prints its TRACEIO_DEVICE line as the
    /// syncIO user before `unlockPort` — the get calls in their C words,
    /// `setInputEos`/`setOutputEos` — and a refused one prints none.
    #[test]
    fn eos_calls_run_under_lock_port_as_asyn_octet_sync_io_does() {
        let dir = tempfile::tempdir().expect("fixture root");
        let (handle, path) = eos_port("sync_eos", dir.path());
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));
        handle
            .port_handle()
            .submit_blocking(RequestOp::SetEnable { yes: false }, AsynUser::default())
            .unwrap();
        assert_eq!(
            sio.write_octet(5, b"x").unwrap_err().status(),
            AsynStatus::Disabled,
            "control: a queued transfer meets the enable gate"
        );
        let start = std::fs::read_to_string(&path).unwrap().len();

        sio.set_input_eos(5, b"\r\n").unwrap();
        assert_eq!(sio.get_input_eos(5).unwrap(), b"\r\n");
        sio.set_output_eos(5, b"\n").unwrap();
        assert_eq!(sio.get_output_eos(5).unwrap(), b"\n");
        assert!(sio.set_input_eos(5, b"abc").is_err());

        // A single-device port prints addr -1 (`getAddr`, asynManager.c:2006).
        let text = std::fs::read_to_string(&path).unwrap();
        let lock = "[sync_eos,-1,5] sync_eos lockPort\n";
        let unlock = "[sync_eos,-1,5] sync_eos unlockPort\n";
        let expected = [
            "[sync_eos,-1,5] sync_eos set Eos 2\n\\r\\n\n[sync_eos,-1,5] asynOctetSyncIO setInputEos eoslen 2\n",
            "[sync_eos,-1,5] sync_eos get Eos 2\n\\r\\n\n[sync_eos,-1,5] asynOctetSyncIO setInputEos eoslen 2\n",
            "[sync_eos,-1,5] sync_eos set Eos 1\n\\n\n[sync_eos,-1,5] asynOctetSyncIO setOutputEos eoslen 1\n",
            "[sync_eos,-1,5] sync_eos get Eos 1\n\\n\n[sync_eos,-1,5] asynOctetSyncIO setOutputEos eoslen 1\n",
            "[sync_eos,-1,5] sync_eos set Eos 3\nabc\n",
        ]
        .map(|body| format!("{lock}{body}{unlock}"))
        .concat();
        assert_eq!(&text[start..], expected);
    }

    /// C `lockPort` refuses a shut-down port with `asynDisabled`
    /// (asynManager.c:1790-1795), and the EOS call returns that status.
    #[test]
    fn eos_calls_on_a_shut_down_port_return_lock_port_s_refusal() {
        let dir = tempfile::tempdir().expect("fixture root");
        let (handle, _) = eos_port("sync_eos_shut", dir.path());
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));
        sio.set_input_eos(0, b"\n").unwrap();
        handle
            .port_handle()
            .submit_blocking(RequestOp::ShutdownPort, AsynUser::default())
            .unwrap();
        for status in [
            sio.set_input_eos(0, b"\n").map(drop),
            sio.get_input_eos(0).map(drop),
            sio.set_output_eos(0, b"\n").map(drop),
            sio.get_output_eos(0).map(drop),
        ] {
            match status {
                Err(AsynError::Status { status, message }) => {
                    assert_eq!(status, AsynStatus::Disabled);
                    assert_eq!(message, "asynManager:lockPort: port has been shut down");
                }
                other => panic!("expected lockPort's refusal, got {other:?}"),
            }
        }
    }

    /// C's direct `pasynOctet->getInputEos` reaches the EOS layer, which prints
    /// its FLOW line on the asking user (asynInterposeEos.c:340-341), so the
    /// unlocked answer prints what the queued one does.
    #[test]
    fn a_direct_get_input_eos_prints_the_eos_layer_s_line() {
        let dir = tempfile::tempdir().expect("fixture root");
        let (handle, path) = eos_port("direct_geos", dir.path());
        let port = handle.port_handle();
        port.set_input_eos_blocking(AsynUser::new(5), b"\r\n")
            .unwrap();
        let since = |start: usize| std::fs::read_to_string(&path).unwrap()[start..].to_owned();

        let start = since(0).len();
        assert_eq!(
            port.get_input_eos_blocking(AsynUser::new(5)).unwrap(),
            b"\r\n"
        );
        let line = "[direct_geos,-1,5] direct_geos get Eos 2\n\\r\\n\n";
        assert!(
            since(start).ends_with(line),
            "control: the queued get prints it"
        );

        let start = since(0).len();
        assert_eq!(port.get_input_eos(&AsynUser::new(5)).unwrap(), b"\r\n");
        assert_eq!(since(start), line);
    }

    /// A driver that answers `getInputEos` itself prints no line
    /// (asynPortDriver.cpp:2782-2788, asynGpib.c:455-467): only the EOS
    /// layer's answer carries one.
    #[test]
    fn a_driver_s_own_get_input_eos_prints_nothing() {
        struct OwnEos(PortDriverBase);
        impl PortDriver for OwnEos {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn get_input_eos(&self, _user: &AsynUser) -> AsynResult<Vec<u8>> {
                Ok(b"\n".to_vec())
            }
        }
        let dir = tempfile::tempdir().expect("fixture root");
        let mut port = TestPort::named("own_geos");
        port.base.flags.destructible = true;
        let (handle, path) = traced_port(OwnEos(port.base), "own_geos", dir.path());
        let port = handle.port_handle();
        let start = std::fs::read_to_string(&path).unwrap().len();
        assert_eq!(port.get_input_eos(&AsynUser::new(5)).unwrap(), b"\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap()[start..], *"");
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
