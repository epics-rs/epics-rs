//! The shape C's areaDetector `writeInt32` / `writeFloat64` overrides share:
//!
//! ```c
//! status = setIntegerParam(function, value);
//! if (function == ...) { ... } else if (function < FIRST_PARAM) status = ADDriver::writeInt32(...);
//! callParamCallbacks();
//! if (status) asynPrint(pasynUser, ASYN_TRACE_ERROR, "%s:%s: error, status=%d function=%d, value=%d\n", ...);
//! else asynPrint(pasynUser, ASYN_TRACEIO_DRIVER, "%s:%s: function=%d, value=%d\n", ...);
//! return status;
//! ```
//!
//! [`WriteTail`] owns the set, the callbacks and the closing line, so the
//! status the line prints is the one the write returns, in C's numbering.

use std::fmt::Display;
use std::panic::Location;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, param_list};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;

/// The list an override's own set and callbacks use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnList {
    /// `setIntegerParam(function, value)` and `callParamCallbacks()`: list 0
    /// whatever the address.
    Zero,
    /// `getAddress(pasynUser, &addr)`, then `setIntegerParam(addr, function,
    /// value)` and `callParamCallbacks(addr)`.
    Addressed,
}

/// One override's closing lines: `"{error}status={code} function={reason},
/// value={value}"` on `ASYN_TRACE_ERROR`, `"{done}function={reason},
/// value={value}"` on `ASYN_TRACEIO_DRIVER`. `error` and `done` are C's text
/// up to `status=` and `function=`, e.g. `"simDetector:writeInt32 error, "`
/// and `"simDetector:writeInt32: "`.
#[derive(Clone, Copy, Debug)]
pub struct WriteTail {
    pub error: &'static str,
    pub done: &'static str,
    pub list: OwnList,
}

/// C's `%f`.
struct CFloat(f64);

impl Display for CFloat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.6}", self.0)
    }
}

impl WriteTail {
    /// `status = setIntegerParam(function, value)`, then `body` with that
    /// status; what `body` returns is the write's status.
    #[track_caller]
    pub fn write_int32<D: PortDriver>(
        &self,
        drv: &mut D,
        user: &mut AsynUser,
        value: i32,
        body: impl FnOnce(&mut D, &mut AsynUser, AsynResult<()>) -> AsynResult<()>,
    ) -> AsynResult<()> {
        let site = Location::caller();
        let addr = self.addr(drv, user);
        let status = drv.base_mut().set_int32_param(user.reason, addr, value);
        let status = body(drv, user, status);
        self.close(drv, user, addr, site, value, status)
    }

    /// `status = setDoubleParam(function, value)`, then `body` with that
    /// status; what `body` returns is the write's status.
    #[track_caller]
    pub fn write_float64<D: PortDriver>(
        &self,
        drv: &mut D,
        user: &mut AsynUser,
        value: f64,
        body: impl FnOnce(&mut D, &mut AsynUser, AsynResult<()>) -> AsynResult<()>,
    ) -> AsynResult<()> {
        let site = Location::caller();
        let addr = self.addr(drv, user);
        let status = drv.base_mut().set_float64_param(user.reason, addr, value);
        let status = body(drv, user, status);
        self.close(drv, user, addr, site, CFloat(value), status)
    }

    fn addr<D: PortDriver>(&self, drv: &D, user: &AsynUser) -> i32 {
        match self.list {
            OwnList::Zero => 0,
            // getAddress prints its refusal and leaves the address for the
            // set to refuse; C ignores its status.
            OwnList::Addressed => {
                let base = drv.base();
                base.get_address(user.addr, user.reason)
                    .unwrap_or_else(|_| param_list(base.flags.multi_device, user.addr))
            }
        }
    }

    fn close<D: PortDriver>(
        &self,
        drv: &mut D,
        user: &AsynUser,
        addr: i32,
        site: &Location<'_>,
        value: impl Display,
        status: AsynResult<()>,
    ) -> AsynResult<()> {
        // C ignores callParamCallbacks' status.
        let _ = drv.base_mut().call_param_callbacks(addr);
        let reason = user.reason;
        match &status {
            Err(e) => user.print(
                TraceMask::ERROR,
                site.file(),
                site.line(),
                format_args!(
                    "{}status={} function={reason}, value={value}\n",
                    self.error,
                    e.c_status_code()
                ),
            ),
            Ok(()) => user.print(
                TraceMask::IO_DRIVER,
                site.file(),
                site.line(),
                format_args!("{}function={reason}, value={value}\n", self.done),
            ),
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use asyn_rs::error::AsynError;
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use asyn_rs::services::PortServices;
    use asyn_rs::trace::TraceFile;
    use asyn_rs::user::UserTrace;

    struct Drv {
        base: PortDriverBase,
        int: usize,
        float: usize,
    }

    impl PortDriver for Drv {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    fn drv(port: &str, max_addr: usize) -> Drv {
        let mut base = PortDriverBase::new(port, max_addr, PortFlags::default());
        let int = base.create_param("INT", ParamType::Int32).unwrap();
        let float = base.create_param("FLOAT", ParamType::Float64).unwrap();
        Drv { base, int, float }
    }

    /// The trace file of `port`, and a user that prints to it.
    fn traced(port: &str, reason: usize) -> (std::path::PathBuf, AsynUser) {
        let manager = PortServices::global().trace().clone();
        let log = std::env::temp_dir().join(format!("{port}-{}", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        manager.set_trace_file(
            Some(port),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        manager.set_trace_mask(Some(port), TraceMask::ERROR | TraceMask::IO_DRIVER);
        let mut user = AsynUser::new(reason);
        user.trace = Some(UserTrace {
            manager,
            port: port.into(),
        });
        (log, user)
    }

    const TAIL: WriteTail = WriteTail {
        error: "drv:writeInt32: error, ",
        done: "drv:writeInt32: ",
        list: OwnList::Zero,
    };

    #[test]
    fn the_own_set_is_list_zero_whatever_the_address() {
        let mut d = drv("TAIL_LIST0", 1);
        let reason = d.int;
        let mut user = AsynUser::new(reason).with_addr(1);
        TAIL.write_int32(&mut d, &mut user, 3, |_, _, s| s).unwrap();
        assert_eq!(d.base.get_int32_param(reason, 0).unwrap(), 3);
    }

    #[test]
    fn an_addressed_set_past_the_lists_is_invalid_list() {
        let mut d = drv("TAIL_ADDRESSED", 2);
        d.base.flags.multi_device = true;
        let tail = WriteTail {
            list: OwnList::Addressed,
            ..TAIL
        };
        let reason = d.int;
        let mut user = AsynUser::new(reason).with_addr(1);
        tail.write_int32(&mut d, &mut user, 3, |_, _, s| s).unwrap();
        assert_eq!(d.base.get_int32_param(reason, 1).unwrap(), 3);
        let mut user = AsynUser::new(reason).with_addr(2);
        let status = tail.write_int32(&mut d, &mut user, 3, |_, _, s| s);
        assert!(matches!(status, Err(AsynError::ParamInvalidList(2))));
    }

    /// getAddress: a single-device port's address is list 0.
    #[test]
    fn an_addressed_set_on_a_single_device_port_is_list_zero() {
        let mut d = drv("TAIL_SINGLE", 1);
        let tail = WriteTail {
            list: OwnList::Addressed,
            ..TAIL
        };
        let reason = d.int;
        let mut user = AsynUser::new(reason).with_addr(1);
        tail.write_int32(&mut d, &mut user, 3, |_, _, s| s).unwrap();
        assert_eq!(d.base.get_int32_param(reason, 0).unwrap(), 3);
    }

    /// The body sees the set's refusal, and the line prints what the body
    /// returns, in C's numbering.
    #[test]
    fn the_line_prints_the_status_the_write_returns() {
        let port = "TAIL_LINE";
        let mut d = drv(port, 1);
        let float = d.float;
        let (log, mut user) = traced(port, float);
        let mut seen = None;
        let status = TAIL.write_int32(&mut d, &mut user, 1, |_, _, s| {
            seen = Some(s.as_ref().map_err(AsynError::c_status_code).err());
            s
        });
        assert!(status.is_err());
        assert_eq!(seen, Some(Some(8)));
        let int = d.int;
        user.reason = int;
        TAIL.write_int32(&mut d, &mut user, 2, |_, _, s| s).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains(&format!(
                "drv:writeInt32: error, status=8 function={float}, value=1"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("drv:writeInt32: function={int}, value=2")),
            "{text}"
        );
    }

    #[test]
    fn a_float_prints_as_cs_percent_f() {
        let port = "TAIL_FLOAT";
        let mut d = drv(port, 1);
        let int = d.int;
        let (log, mut user) = traced(port, int);
        let tail = WriteTail {
            error: "drv:writeFloat64 error, ",
            done: "drv:writeFloat64: ",
            list: OwnList::Zero,
        };
        assert!(
            tail.write_float64(&mut d, &mut user, 1.5, |_, _, s| s)
                .is_err()
        );
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains(&format!(
                "drv:writeFloat64 error, status=8 function={int}, value=1.500000"
            )),
            "{text}"
        );
    }

    /// The body's own replacement status is what the write returns.
    #[test]
    fn the_body_replaces_the_status() {
        let mut d = drv("TAIL_BODY", 1);
        let mut user = AsynUser::new(d.float);
        TAIL.write_int32(&mut d, &mut user, 1, |_, _, _| Ok(()))
            .unwrap();
    }
}
