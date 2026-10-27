//! `devXxStrParm.c` — the `asyn <xx> stringParm` device support: one octet
//! read or write per process, its text formatted or scanned with a C format.
//!
//! The link is `@asyn(port addr)TERM=..,IX=..,FMT=..,TO=..,N=..,0STR=..,1STR=..`;
//! a record with a `DESC` uses it as the format, which `FMT=` then overrides.
//! Inputs read up to `N` bytes ending in `TERM`, and scan from offset `IX`;
//! outputs write the formatted value, terminated by `TERM` as the output EOS.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use asyn_rs::adapter::parse_asyn_link;
use asyn_rs::error::{AsynError, AsynStatus};
use asyn_rs::manager::PortManager;
use asyn_rs::port::QueuePriority;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::{CellBody, OctetReadOp, PortCalls, RequestCell, RequestOp, Unrun};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::{AsynUser, timeout_from_secs};
use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::epics_string::print_escaped;
use epics_base_rs::runtime::log::errlog_vprintf;
use epics_base_rs::runtime::scanf::{self, ScanTarget};
use epics_base_rs::runtime::stdlib::strtod;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart, WriteStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::c_parse::atoi as c_atoi;
use epics_libcom_rs::cfmt::CFloat;

use crate::Unconnected;
use crate::fmt::{c_literal, format_c_double, format_c_int};
use crate::user_param::{after, c_str, term, up_to_comma};

/// The `device(<type>,INST_IO,dev<Xx>StrParm,"asyn <xx> stringParm")` lines
/// of `ipSupport.dbd`.
pub const DEVICES: [(&str, &str); 8] = [
    ("ai", "asyn ai stringParm"),
    ("ao", "asyn ao stringParm"),
    ("bi", "asyn bi stringParm"),
    ("bo", "asyn bo stringParm"),
    ("longin", "asyn li stringParm"),
    ("longout", "asyn lo stringParm"),
    ("stringin", "asyn si stringParm"),
    ("stringout", "asyn so stringParm"),
];

/// [`DEVICES`] as device menu rows, for `register_dynamic_device_support`.
/// The dsets are `{6, 0, 0, init, 0, io, convert}`, convert only on ai/ao.
pub static DEVICE_ROWS: [(&str, &[DevSup]); 8] = [
    ("ai", &[row("devAiStrParm", 0, &[2, 4, 5])]),
    ("ao", &[row("devAoStrParm", 1, &[2, 4, 5])]),
    ("bi", &[row("devBiStrParm", 2, &[2, 4])]),
    ("bo", &[row("devBoStrParm", 3, &[2, 4])]),
    ("longin", &[row("devLiStrParm", 4, &[2, 4])]),
    ("longout", &[row("devLoStrParm", 5, &[2, 4])]),
    ("stringin", &[row("devSiStrParm", 6, &[2, 4])]),
    ("stringout", &[row("devSoStrParm", 7, &[2, 4])]),
];

const fn row(name: &'static str, device: usize, funcs: &'static [u8]) -> DevSup {
    DevSup {
        name,
        choice: DEVICES[device].1,
        link_type: DbLinkType::InstIo,
        dset: Dset::new(6, funcs, false),
    }
}

/// Dynamic device-support factory for the eight `stringParm` dsets. The link
/// is parsed in `init`, where C's `initCommon` parses it.
pub fn device_support_factory()
-> impl Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync + 'static {
    |ctx: &DeviceSupportContext| {
        let kind = Kind::of(ctx.record_type).filter(|k| ctx.dtyp == k.dtyp())?;
        let link = if kind.is_output() { ctx.out } else { ctx.inp };
        Some(Box::new(DevStrParm::new(kind, link)) as Box<dyn DeviceSupport>)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ai,
    Ao,
    Bi,
    Bo,
    Li,
    Lo,
    Si,
    So,
}

impl Kind {
    pub fn of(record_type: &str) -> Option<Self> {
        Some(match record_type {
            "ai" => Self::Ai,
            "ao" => Self::Ao,
            "bi" => Self::Bi,
            "bo" => Self::Bo,
            "longin" => Self::Li,
            "longout" => Self::Lo,
            "stringin" => Self::Si,
            "stringout" => Self::So,
            _ => return None,
        })
    }

    fn dtyp(self) -> &'static str {
        DEVICES[self as usize].1
    }

    fn is_output(self) -> bool {
        matches!(self, Self::Ao | Self::Bo | Self::Lo | Self::So)
    }

    /// The format `initAi` … `initSo` hand `initCommon`.
    fn default_format(self) -> &'static str {
        match self {
            Self::Ai => "%lf",
            Self::Ao => "%f",
            Self::Li => "%ld",
            Self::Si | Self::So => "%s",
            Self::Bi | Self::Bo | Self::Lo => "%d",
        }
    }
}

/// `devStrParmPvt`'s configuration, as `initCommon` fills it.
#[derive(Debug, Clone, PartialEq)]
struct Parm {
    term: Vec<u8>,
    /// `bufferStartIndex`.
    index: i32,
    format: String,
    /// Seconds; `TO=` is milliseconds.
    timeout: f64,
    nchar: i32,
    zero: String,
    one: String,
}

impl Parm {
    fn parse(user_param: &str, desc: &str, default_format: &str) -> Self {
        let mut parm = Self {
            term: b"\r\n".to_vec(),
            index: 0,
            format: if desc.is_empty() {
                default_format.to_owned()
            } else {
                desc.chars().take(31).collect()
            },
            timeout: 1.0,
            nchar: 100,
            zero: String::new(),
            one: String::new(),
        };
        if let Some(hex) = after(user_param, "TERM=") {
            parm.term = term(hex);
        }
        if let Some(v) = after(user_param, "IX=") {
            parm.index = c_atoi(v.as_bytes());
        }
        if let Some(v) = after(user_param, "FMT=") {
            parm.format = up_to_comma(v);
        }
        if let Some(v) = after(user_param, "TO=") {
            parm.timeout = strtod(v.as_bytes()).value / 1000.;
        }
        if let Some(v) = after(user_param, "N=") {
            parm.nchar = c_atoi(v.as_bytes());
        }
        if let Some(v) = after(user_param, "0STR=") {
            parm.zero = up_to_comma(v);
        }
        if let Some(v) = after(user_param, "1STR=") {
            parm.one = up_to_comma(v);
        }
        parm
    }

    /// `bufferStartIndex` as an offset into `len` bytes: C compares the `int`
    /// against a `size_t`, so a negative one is never inside.
    fn start_in(&self, len: usize) -> Option<usize> {
        usize::try_from(self.index).ok().filter(|&i| i < len)
    }
}

/// What `devStrParmCallback` leaves in `devStrParmPvt`.
#[derive(Default)]
struct Exchange {
    /// `buffer` for a write.
    send: Vec<u8>,
    /// `buffer[0..nread]` for a read.
    read: Vec<u8>,
    /// `pPvt->status`, as C's asynStatus number.
    status: i32,
}

/// The queued request's body — `devStrParmCallback`.
struct Job {
    name: String,
    output: bool,
    term: Vec<u8>,
    nchar: usize,
    timeout: f64,
    addr: i32,
    handle: PortHandle,
    exchange: Mutex<Exchange>,
    /// The record's completion and band.
    reentry: Mutex<Option<(DeviceDone, CallbackPriority)>>,
}

impl Job {
    fn arm(&self, done: &DeviceDoneSource<'_>) {
        let mut slot = self.reentry.lock().unwrap();
        match &mut *slot {
            Some((_, prio)) => *prio = done.priority(),
            None => *slot = Some((done.device_done(), done.priority())),
        }
    }

    fn user(&self) -> AsynUser {
        AsynUser::new(0)
            .with_addr(self.addr)
            .with_timeout_opt(timeout_from_secs(self.timeout))
    }

    fn eos_failed(&self, which: &str, e: &AsynError) {
        print(
            &self.handle,
            self.addr,
            TraceMask::ERROR,
            format_args!(
                "devStrParmCallback, {} error calling {which} {}\n",
                self.name,
                e.message()
            ),
        );
    }
}

impl CellBody for Job {
    fn process(&self, port: &mut dyn PortCalls) {
        let mut ex = self.exchange.lock().unwrap();
        let eos = self.term.clone();
        if self.output {
            if let Err(e) = port.call(&RequestOp::SetOutputEos { eos }, self.user()) {
                self.eos_failed("setOutputEos", &e);
            }
            let data = c_str(&ex.send).to_vec();
            ex.status = match port.call(&RequestOp::OctetWrite { data }, self.user()) {
                Ok(_) => 0,
                Err(e) => e.c_status_code(),
            };
        } else {
            if let Err(e) = port.call(&RequestOp::SetInputEos { eos }, self.user()) {
                self.eos_failed("setInputEos", &e);
            }
            let (read, status) = port
                .octet_read(
                    OctetReadOp::Read {
                        buf_size: self.nchar,
                    },
                    self.user(),
                )
                .into_parts();
            ex.read = read;
            ex.status = status.map_or_else(|e| e.c_status_code(), |()| 0);
        }
    }

    /// No `timeoutUser`, so only a port shutting down with the request queued
    /// lands here; the pass completes as a failed transfer.
    fn unrun(&self, why: Unrun) {
        let mut ex = self.exchange.lock().unwrap();
        ex.read.clear();
        ex.status = match why {
            Unrun::Refused(e) => e.c_status_code(),
            _ => AsynStatus::Error as i32,
        };
    }

    /// `dbScanLock(pr); prset->process(pr)` at the end of the callback.
    fn done(self: Arc<Self>) {
        let armed = self.reentry.lock().unwrap().clone();
        if let Some((done, prio)) = armed {
            let _ = if self.output {
                done.complete(prio)
            } else {
                done.reprocess(prio)
            };
        }
    }

    /// `createAsynUser(devStrParmCallback, 0)`.
    fn timeout_user(&self) -> bool {
        false
    }
}

/// `initCommon`'s `bad:` arm after a failed asynManager call: the line its
/// `asynPrint` writes on the still unconnected asynUser, then `pact = 1`.
fn bad(name: &str, message: &str) -> DeviceInitOutcome {
    PortManager::global().trace_manager().print(
        None,
        None,
        0,
        TraceMask::ERROR,
        format_args!("{name} asynManager error {message}\n"),
    );
    DeviceInitOutcome::dead()
}

/// `asynPrint` on the record's asynUser, connected to `port` at `addr`.
fn print(handle: &PortHandle, addr: i32, mask: TraceMask, args: std::fmt::Arguments<'_>) {
    handle
        .trace()
        .print(Some(handle.port_name()), Some(addr), 0, mask, args);
}

struct Live {
    job: Arc<Job>,
    cell: Arc<RequestCell>,
    parm: Parm,
}

pub struct DevStrParm {
    kind: Kind,
    link: String,
    /// C `pr->name`.
    name: String,
    live: Option<Live>,
}

fn field_f64(record: &dyn Record, field: &str) -> f64 {
    match record.get_field(field) {
        Some(EpicsValue::Double(v)) => v,
        Some(EpicsValue::Long(v)) => f64::from(v),
        Some(EpicsValue::Enum(v)) => f64::from(v),
        Some(EpicsValue::Short(v)) => f64::from(v),
        _ => 0.0,
    }
}

impl DevStrParm {
    pub fn new(kind: Kind, link: &str) -> Self {
        Self {
            kind,
            link: link.to_owned(),
            name: String::new(),
            live: None,
        }
    }

    fn print(&self, mask: TraceMask, args: std::fmt::Arguments<'_>) {
        if let Some(live) = &self.live {
            print(&live.job.handle, live.job.addr, mask, args);
        }
    }

    /// `writeXx` with `pact == 0`: `buffer`. C `sprintf`s whatever the format
    /// says; a conversion of the wrong type is undefined there and sends
    /// nothing here.
    fn buffer(&self, parm: &Parm, record: &dyn Record) -> Vec<u8> {
        let value = field_f64(record, "VAL");
        let formatted = match self.kind {
            Kind::Ao => format_c_double(&parm.format, value),
            Kind::Bo | Kind::Lo => format_c_int(&parm.format, value as i32),
            _ => {
                // `strncpy(buffer, &val[bufferStartIndex], sizeof(val))`.
                let val = match record.get_field("VAL") {
                    Some(EpicsValue::String(s)) => s.to_string().into_bytes(),
                    _ => Vec::new(),
                };
                let at = usize::try_from(parm.index).unwrap_or(usize::MAX);
                return val.get(at..).map(<[u8]>::to_vec).unwrap_or_default();
            }
        };
        match formatted {
            Ok(text) => text.into_bytes(),
            Err(e) => match c_literal(&parm.format) {
                Some(text) => text.into_bytes(),
                None => {
                    self.print(
                        TraceMask::ERROR,
                        format_args!("devXxStrParm {} {e}\n", self.name),
                    );
                    Vec::new()
                }
            },
        }
    }

    /// `startIOCommon`. A refused `queueRequest` returns -1 with PACT already
    /// set, so the record stays active, as in C.
    fn start(&mut self, record: &dyn Record, done: &DeviceDoneSource<'_>) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if self.kind.is_output() {
            let send = self.buffer(&live.parm, record);
            live.job.exchange.lock().unwrap().send = send;
        }
        live.job.arm(done);
        let user = AsynUser::new(0)
            .with_addr(live.job.addr)
            .with_priority(QueuePriority::Low);
        let _ = live.job.handle.queue_request(&live.cell, &user);
    }

    /// `completeIOCommon` for an input.
    fn complete_input(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let Some(live) = self.live.as_ref() else {
            return Ok(DeviceReadOutcome::ok());
        };
        let (status, buffer) = {
            let ex = live.job.exchange.lock().unwrap();
            (ex.status, ex.read.clone())
        };
        let parm = &live.parm;
        let name = &self.name;
        let nread = buffer.len();
        self.print(
            TraceMask::FLOW,
            format_args!("devXxStrParm::completeIOCommon ({name}) status={status} nread={nread}\n"),
        );
        live.job.handle.trace().output_device_io(
            live.job.handle.port_name(),
            Some(live.job.addr),
            0,
            TraceMask::IO_DEVICE,
            &buffer,
            "devXxStrParm::completeIOCommon\n",
            file!(),
            line!(),
        );
        // "Timeout is expected if no terminator".
        let refused = !parm.term.is_empty() && status != 0;
        let source = parm
            .start_in(nread)
            .filter(|_| !refused)
            .map(|at| c_str(&buffer[at..]));
        // C's `sscanf(buffer, format, &prec->val)`: the field is the one
        // destination, and `()` the argument C did not pass.
        let scan = |source: &[u8], val: &dyn ScanTarget| {
            scanf::sscanf(source, parm.format.as_bytes(), &[val, &()])
        };
        let flow = |n: i32, val: &dyn std::fmt::Display| {
            self.print(
                TraceMask::FLOW,
                format_args!("devXxStrParm: record={name}, sscanf returns={n}, val={val}\n"),
            );
        };
        let udf = |undefined: bool| {
            if undefined {
                DeviceUdf::Undefined
            } else {
                DeviceUdf::Defined
            }
        };
        let defined = match (self.kind, source) {
            (Kind::Ai, Some(source)) => {
                let val = Cell::new(field_f64(record, "VAL"));
                let n = scan(source, &val);
                record.put_field("VAL", EpicsValue::Double(val.get()))?;
                flow(n, &format_args!("{:.6}", CFloat(val.get())));
                n == 1
            }
            (Kind::Bi, Some(source)) => {
                // C defect not copied: C tests the record's UDF from the
                // previous pass to decide whether `0STR=` matched
                // (devXxStrParm.c:465, :471), and returns 0 (:479), so
                // biRecord then overwrites VAL from an RVAL nothing set: VAL
                // never takes the reply. Here a match is local and VAL is the
                // result.
                let matched = if !parm.zero.is_empty() && source.starts_with(parm.zero.as_bytes()) {
                    Some(0)
                } else if !parm.one.is_empty() && source.starts_with(parm.one.as_bytes()) {
                    Some(1)
                } else {
                    None
                };
                let (val, defined) = match matched {
                    Some(v) => (v, true),
                    None => {
                        let val = Cell::new(match record.get_field("VAL") {
                            Some(EpicsValue::Enum(v)) => v,
                            _ => 0,
                        });
                        let n = scan(source, &val);
                        flow(n, &val.get());
                        (val.get(), n == 1)
                    }
                };
                record.put_field("VAL", EpicsValue::Enum(val))?;
                defined
            }
            (Kind::Li, Some(source)) => {
                let val = Cell::new(match record.get_field("VAL") {
                    Some(EpicsValue::Long(v)) => v,
                    _ => 0,
                });
                let n = scan(source, &val);
                record.put_field("VAL", EpicsValue::Long(val.get()))?;
                flow(n, &format_args!("{:.6}", CFloat(field_f64(record, "VAL"))));
                n == 1
            }
            (Kind::Si, Some(source)) => {
                let text = String::from_utf8_lossy(&source[..source.len().min(39)]);
                record.put_field("VAL", EpicsValue::String(text.as_ref().into()))?;
                true
            }
            (Kind::Ai, None) => {
                record.put_field("VAL", EpicsValue::Double(0.0))?;
                false
            }
            (Kind::Bi, None) => {
                record.put_field("VAL", EpicsValue::Enum(0))?;
                false
            }
            (Kind::Li, None) => {
                record.put_field("VAL", EpicsValue::Long(0))?;
                false
            }
            (Kind::Si, None) => {
                record.put_field("VAL", EpicsValue::String("".into()))?;
                false
            }
            _ => return Ok(DeviceReadOutcome::ok()),
        };
        let udf = udf(!defined);
        // The input arm falls through into the output arm's
        // `if (pPvt->status != 0) rc = -1`.
        Ok(if status != 0 {
            DeviceReadOutcome::no_value(udf)
        } else {
            match self.kind {
                Kind::Ai | Kind::Bi => DeviceReadOutcome::computed(udf),
                _ => DeviceReadOutcome::converted(udf),
            }
        })
    }
}

impl DeviceSupport for DevStrParm {
    fn dtyp(&self) -> &str {
        self.kind.dtyp()
    }

    fn holds_dpvt(&self) -> bool {
        self.live.is_some()
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.name = name.to_owned();
    }

    /// C `initCommon`. Every refusal is its `bad:` arm, `pr->pact = 1`.
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        let name = self.name.clone();
        let link = match parse_asyn_link(&self.link) {
            Ok(link) => link,
            Err(e) => {
                let message = match e {
                    AsynError::InvalidLinkSyntax(m) => m,
                    other => other.to_string(),
                };
                errlog_vprintf(format_args!(
                    "devXxStrParm::initCommon {name} bad link {message}\n"
                ));
                return Ok(bad(&name, &message));
            }
        };
        let handle = match crate::connect_octet(&link.port_name, link.addr) {
            Ok(handle) => handle,
            Err(Unconnected::Device(message)) => return Ok(bad(&name, &message)),
            Err(Unconnected::NoOctet) => return Ok(DeviceInitOutcome::dead()),
        };
        let desc = match record.get_field("DESC") {
            Some(EpicsValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        let user_param = link.drv_info.as_str();
        let parm = Parm::parse(user_param, &desc, self.kind.default_format());
        let addr = link.addr;
        let trace = |args: std::fmt::Arguments<'_>| {
            print(&handle, addr, TraceMask::IO_DEVICE, args);
        };
        trace(format_args!(
            "devXxStrParm::initCommon {name} userParam = '{user_param}'\n"
        ));
        let term: String = print_escaped(&parm.term).chars().take(19).collect();
        trace(format_args!("   term = {term}\n"));
        trace(format_args!("   bufferStartIndex = {}\n", parm.index));
        trace(format_args!("   timeout = {:.6}\n", CFloat(parm.timeout)));
        trace(format_args!("   format = {}\n", parm.format));
        trace(format_args!("   nchar = {}\n", parm.nchar));
        let job = Arc::new(Job {
            name,
            output: self.kind.is_output(),
            term: parm.term.clone(),
            nchar: usize::try_from(parm.nchar).unwrap_or(0),
            timeout: parm.timeout,
            addr,
            handle: handle.clone(),
            exchange: Mutex::new(Exchange::default()),
            reentry: Mutex::new(None),
        });
        let cell = handle.new_cell(job.clone());
        self.live = Some(Live { job, cell, parm });
        Ok(DeviceInitOutcome::ok())
    }

    fn read_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        if done.pact() || self.live.is_none() {
            return Ok(ReadStart::Synchronous);
        }
        self.start(record, done);
        Ok(ReadStart::Pending)
    }

    /// C `strParmConvert`, the slot of both devAiStrParm and devAoStrParm.
    fn has_special_linconv(&self) -> bool {
        matches!(self.kind, Kind::Ai | Kind::Ao)
    }

    /// `strParmConvert` writes through an `aiRecord *` cast (`devXxStrParm.c:534`), which on
    /// an ao lands on LOPR and the low half of ASLO; that C defect is not
    /// copied, so an ao keeps only the record's EOFF rebase.
    fn special_linconv(&mut self, record: &mut dyn Record, _after: bool) -> CaResult<()> {
        if self.kind == Kind::Ai {
            crate::unit_slope(record)?;
        }
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.complete_input(record)
    }

    fn write_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        if self.live.is_none() {
            return Ok(WriteStart::Completed);
        }
        self.start(record, done);
        Ok(WriteStart::Pending)
    }

    /// A failed write is the -1 the output records do not look at.
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use asyn_rs::error::AsynResult;
    use asyn_rs::interpose::EomReason;
    use asyn_rs::request::{OctetRead, RequestResult};
    use epics_base_rs::server::records::ai::AiRecord;
    use epics_base_rs::server::records::ao::AoRecord;
    use epics_base_rs::server::records::bi::BiRecord;
    use epics_base_rs::server::records::stringin::StringinRecord;
    use epics_base_rs::server::records::stringout::StringoutRecord;

    use super::*;

    fn live_dev(kind: Kind, user_param: &str, desc: &str, port: &str) -> (DevStrParm, PortHandle) {
        use asyn_rs::drivers::ip_port::DrvAsynIPPort;
        use asyn_rs::manager::{PortManager, claim_port_driver};
        use asyn_rs::port::PortDriver;
        use asyn_rs::registry::open_owner;

        let mut driver = DrvAsynIPPort::new(port, "127.0.0.1:1").unwrap();
        driver.base_mut().init_auto_connect(false);
        let runtime = PortManager::new()
            .register_port(claim_port_driver(port).unwrap(), driver, &open_owner())
            .unwrap();
        let handle = runtime.port_handle().clone();
        std::mem::forget(runtime);
        let parm = Parm::parse(user_param, desc, kind.default_format());
        let job = Arc::new(Job {
            name: "R:x".into(),
            output: kind.is_output(),
            term: parm.term.clone(),
            nchar: usize::try_from(parm.nchar).unwrap_or(0),
            timeout: parm.timeout,
            addr: 0,
            handle: handle.clone(),
            exchange: Mutex::new(Exchange::default()),
            reentry: Mutex::new(None),
        });
        let mut dev = DevStrParm::new(kind, "");
        dev.live = Some(Live {
            cell: handle.new_cell(job.clone()),
            job,
            parm,
        });
        (dev, handle)
    }

    /// A port that logs each op and answers a read with `reply`, failed with
    /// a timeout after those bytes when `timeout` is set.
    struct Wire {
        log: Vec<String>,
        reply: Vec<u8>,
        timeout: bool,
    }

    impl Wire {
        fn new(reply: &[u8], timeout: bool) -> Self {
            Self {
                log: Vec::new(),
                reply: reply.to_vec(),
                timeout,
            }
        }
    }

    impl PortCalls for Wire {
        fn call(&mut self, op: &RequestOp, _user: AsynUser) -> AsynResult<RequestResult> {
            self.log.push(match op {
                RequestOp::SetInputEos { eos } => format!("ieos {eos:?}"),
                RequestOp::SetOutputEos { eos } => format!("oeos {eos:?}"),
                RequestOp::OctetWrite { data } => {
                    format!("write {:?}", String::from_utf8_lossy(data))
                }
                RequestOp::OctetRead { buf_size, .. } => format!("read {buf_size}"),
                other => format!("{other:?}"),
            });
            match op {
                RequestOp::OctetRead { .. } => {
                    let status = if self.timeout {
                        Err(AsynError::Status {
                            status: AsynStatus::Timeout,
                            message: "timeout".into(),
                        })
                    } else {
                        Ok(())
                    };
                    let read = OctetRead::new(self.reply.clone(), EomReason::empty(), status);
                    Ok(RequestResult::octet_read(read, 0))
                }
                _ => Ok(RequestResult::write_ok()),
            }
        }
    }

    fn run(dev: &DevStrParm, wire: &mut Wire) {
        dev.live.as_ref().unwrap().job.process(wire);
    }

    /// `initCommon`'s userParam keys, and DESC as the format under `FMT=`.
    #[test]
    fn user_param_parses_as_init_common_does() {
        let p = Parm::parse("TERM=,IX=3,FMT=%lf,N=10", "", "%lf");
        assert_eq!((p.term.as_slice(), p.index, p.nchar), (&b""[..], 3, 10));
        assert_eq!((p.format.as_str(), p.timeout), ("%lf", 1.0));
        let p = Parm::parse("TERM=0d0a3,TO=250,0STR=OFF,1STR=ON", "V=%lf", "%lf");
        assert_eq!(p.term, b"\r\n");
        assert_eq!((p.format.as_str(), p.timeout), ("V=%lf", 0.25));
        assert_eq!((p.zero.as_str(), p.one.as_str()), ("OFF", "ON"));
        let p = Parm::parse("", "", "%d");
        assert_eq!(
            (p.term.as_slice(), p.nchar, p.format.as_str()),
            (&b"\r\n"[..], 100, "%d")
        );
    }

    /// The input callback: `setInputEos(term)` then a read of `N` bytes,
    /// scanned from `IX`.
    #[test]
    fn an_ai_reads_n_bytes_and_scans_from_ix() {
        let (mut dev, _h) = live_dev(Kind::Ai, "TERM=,IX=3,FMT=%lf,N=10", "", "SP_AI");
        let mut wire = Wire::new(b"\x02SL 123.5", false);
        run(&dev, &mut wire);
        assert_eq!(wire.log, ["ieos []", "read 10"]);
        let mut rec = AiRecord::default();
        let out = dev.read(&mut rec).unwrap();
        assert_eq!(rec.val, 123.5);
        assert!(out.did_compute());
        assert_eq!(out.udf(), DeviceUdf::Defined);
    }

    /// With no terminator a timeout still scans what arrived, and the
    /// fall-through into the output arm makes it -1.
    #[test]
    fn an_unterminated_timeout_scans_what_arrived_and_fails() {
        let (mut dev, _h) = live_dev(Kind::Ai, "TERM=,IX=3,N=10", "", "SP_AI_TMO");
        run(&dev, &mut Wire::new(b"xxx42", true));
        let mut rec = AiRecord::default();
        let out = dev.read(&mut rec).unwrap();
        assert_eq!(rec.val, 42.0);
        assert!(out.status.read_failed());
        assert_eq!(out.udf(), DeviceUdf::Defined);

        let (mut dev, _h) = live_dev(Kind::Ai, "IX=0", "", "SP_AI_TERM");
        run(&dev, &mut Wire::new(b"42", true));
        let mut rec = AiRecord::default();
        rec.val = 7.0;
        let out = dev.read(&mut rec).unwrap();
        assert_eq!(rec.val, 0.0);
        assert_eq!(out.udf(), DeviceUdf::Undefined);
    }

    /// A failed scan leaves VAL and marks it undefined; an index past the
    /// reply zeroes it.
    #[test]
    fn a_failed_scan_marks_val_undefined() {
        let (mut dev, _h) = live_dev(Kind::Ai, "", "", "SP_AI_BAD");
        run(&dev, &mut Wire::new(b"abc", false));
        let mut rec = AiRecord::default();
        rec.val = 7.0;
        let out = dev.read(&mut rec).unwrap();
        assert_eq!((rec.val, out.udf()), (7.0, DeviceUdf::Undefined));

        let (mut dev, _h) = live_dev(Kind::Ai, "IX=5", "", "SP_AI_IX");
        run(&dev, &mut Wire::new(b"12", false));
        let out = dev.read(&mut rec).unwrap();
        assert_eq!((rec.val, out.udf()), (0.0, DeviceUdf::Undefined));
    }

    /// The format goes to glibc's `sscanf` with the field as its one
    /// destination, so a literal `%%` matches and a second conversion makes
    /// the count 2.
    #[test]
    fn the_format_scans_as_glibc_sscanf() {
        let ai = |fmt: &str, reply: &[u8], port: &str| {
            let (mut dev, _h) = live_dev(Kind::Ai, &format!("TERM=,FMT={fmt}"), "", port);
            run(&dev, &mut Wire::new(reply, false));
            let mut rec = AiRecord::default();
            rec.val = 7.0;
            let out = dev.read(&mut rec).unwrap();
            (rec.val, out.udf())
        };
        assert_eq!(ai("%lf%%", b"2.5%", "SP_AI_PCT"), (2.5, DeviceUdf::Defined));
        assert_eq!(
            ai("%lf %lf", b"1 2", "SP_AI_TWO"),
            (1.0, DeviceUdf::Undefined)
        );
        assert_eq!(ai("%lf", b"1e", "SP_AI_EXP"), (7.0, DeviceUdf::Undefined));
    }

    /// `0STR=` / `1STR=` before the scan.
    #[test]
    fn a_bi_matches_its_strings_then_scans() {
        let bi = |reply: &[u8], port: &str| {
            let (mut dev, _h) = live_dev(Kind::Bi, "0STR=OFF,1STR=ON", "", port);
            run(&dev, &mut Wire::new(reply, false));
            let mut rec = BiRecord::default();
            let out = dev.read(&mut rec).unwrap();
            (rec.get_field("VAL"), out.udf())
        };
        assert_eq!(
            bi(b"OFF", "SP_BI0"),
            (Some(EpicsValue::Enum(0)), DeviceUdf::Defined)
        );
        assert_eq!(
            bi(b"ON", "SP_BI1"),
            (Some(EpicsValue::Enum(1)), DeviceUdf::Defined)
        );
        assert_eq!(
            bi(b"1", "SP_BI2"),
            (Some(EpicsValue::Enum(1)), DeviceUdf::Defined)
        );
        assert_eq!(bi(b"?", "SP_BI3").1, DeviceUdf::Undefined);
    }

    #[test]
    fn a_stringin_takes_the_reply_from_ix() {
        let (mut dev, _h) = live_dev(Kind::Si, "IX=2", "", "SP_SI");
        run(&dev, &mut Wire::new(b"> READY\0junk", false));
        let mut rec = StringinRecord::default();
        dev.read(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::String("READY".into()))
        );
    }

    /// The output callback: `setOutputEos(term)` then the formatted value.
    #[test]
    fn outputs_write_the_formatted_value_after_setting_the_output_eos() {
        let (dev, _h) = live_dev(Kind::Ao, "FMT=SL%4.0lf,TERM=03", "", "SP_AO");
        let mut ao = AoRecord::default();
        ao.val = 123.4;
        let live = dev.live.as_ref().unwrap();
        live.job.exchange.lock().unwrap().send = dev.buffer(&live.parm, &ao);
        let mut wire = Wire::new(b"", false);
        run(&dev, &mut wire);
        assert_eq!(wire.log, ["oeos [3]", "write \"SL 123\""]);

        let (dev, _h) = live_dev(Kind::So, "IX=1", "", "SP_SO");
        let mut so = StringoutRecord::default();
        so.put_field("VAL", EpicsValue::String("XPV".into()))
            .unwrap();
        let live = dev.live.as_ref().unwrap();
        assert_eq!(dev.buffer(&live.parm, &so), b"PV");
    }

    #[test]
    fn the_factory_serves_the_eight_string_parm_dsets() {
        let factory = device_support_factory();
        for (rtyp, dtyp) in DEVICES {
            let ctx = DeviceSupportContext {
                record_type: rtyp,
                dtyp,
                inp: "@asyn(P)",
                out: "@asyn(P)",
            };
            assert!(factory(&ctx).is_some(), "{rtyp}");
        }
        let ctx = DeviceSupportContext {
            record_type: "ai",
            dtyp: "asyn bi stringParm",
            inp: "",
            out: "",
        };
        assert!(factory(&ctx).is_none());
    }

    /// `initCommon` on `link`, the global asyn trace (an unconnected
    /// asynUser's) captured: the outcome and what was printed.
    fn init_traced(link: &str, name: &str) -> (DeviceInitOutcome, String) {
        use asyn_rs::trace::TraceFile;

        let path = std::env::temp_dir().join(format!("strparm-{name}-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let trace = PortManager::global().trace_manager().clone();
        trace.set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));
        let mut dev = DevStrParm::new(Kind::Ai, link);
        dev.set_record_info(name, ScanType::Passive);
        let outcome = dev.init(&mut AiRecord::default()).unwrap();
        trace.set_trace_file(None, TraceFile::Stderr);
        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        (outcome, log)
    }

    /// `initCommon`'s `bad:` arm prints the failed `connectDevice`'s or
    /// `parseLink`'s errorMessage (devXxStrParm.c:231-236) before
    /// `pact = 1`.
    #[test]
    fn a_failed_connect_or_link_prints_the_asyn_manager_error() {
        let (outcome, log) = init_traced("@asyn(SP_NO_SUCH_PORT 0)", "R:np");
        assert_eq!(outcome, DeviceInitOutcome::dead());
        assert!(
            log.trim_end().ends_with(
                "R:np asynManager error asynManager:connectDevice port SP_NO_SUCH_PORT not found"
            ),
            "{log:?}"
        );
        let message = match parse_asyn_link("not a link") {
            Err(AsynError::InvalidLinkSyntax(m)) => m,
            other => panic!("{other:?}"),
        };
        let (outcome, log) = init_traced("not a link", "R:bl");
        assert_eq!(outcome, DeviceInitOutcome::dead());
        assert!(
            log.trim_end()
                .ends_with(&format!("R:bl asynManager error {message}")),
            "{log:?}"
        );
    }

    /// A port without `asynOctet` fails `findInterface` with asynSuccess:
    /// `bad:` prints nothing and sets `pact = 1` (devXxStrParm.c:145-146).
    #[test]
    fn a_port_without_octet_kills_the_record_silently() {
        crate::no_octet_port("SP_INT32_ONLY");
        let (outcome, log) = init_traced("@asyn(SP_INT32_ONLY 0)", "R:i32");
        assert_eq!(outcome, DeviceInitOutcome::dead());
        assert_eq!(log, "");
    }
}
