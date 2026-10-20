//! Trace/logging system (asynTrace equivalent).
//!
//! Provides per-port configurable tracing with support for multiple output
//! destinations, I/O data formatting, and bitflag-based mask filtering.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bitflags::bitflags;
use epics_libcom_rs::runtime::log::{
    StagedLine, StagedStderr, errlog_printf_bytes, errlog_vprintf,
};

use crate::exception::{AsynException, ExceptionEvent, ExceptionManager};

bitflags! {
    /// What to trace — control message categories.
    ///
    /// Values match C asyn `asynDriver.h:211-216` exactly:
    /// ```text
    ///   ASYN_TRACE_ERROR     0x0001
    ///   ASYN_TRACEIO_DEVICE  0x0002
    ///   ASYN_TRACEIO_FILTER  0x0004
    ///   ASYN_TRACEIO_DRIVER  0x0008
    ///   ASYN_TRACE_FLOW      0x0010
    ///   ASYN_TRACE_WARNING   0x0020
    /// ```
    /// The C mask is a plain `int`, and drivers define bits of their own
    /// (`ASYN_TRACE_INFO 0x0040` in twincat and motorNewport), so every bit
    /// is kept, not only the six named ones.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceMask: u32 {
        const ERROR      = 0x0001;
        const IO_DEVICE  = 0x0002;
        const IO_FILTER  = 0x0004;
        const IO_DRIVER  = 0x0008;
        const FLOW       = 0x0010;
        const WARNING    = 0x0020;
        const _ = !0;
    }
}

impl TraceMask {
    /// C `asynTraceMaskStringToInt` (asynShellCommands.c:670-699): the bits
    /// read and the text the parse stopped at, empty when all of it parsed.
    /// C applies the bits even when a tail is left.
    pub fn from_c_mask_string(s: &str) -> (TraceMask, &str) {
        let (bits, rest) = c_mask_string_to_int(
            s,
            &["TRACE_", "TRACEIO_"],
            &[
                ("ERROR", Self::ERROR.bits()),
                ("DEVICE", Self::IO_DEVICE.bits()),
                ("FILTER", Self::IO_FILTER.bits()),
                ("DRIVER", Self::IO_DRIVER.bits()),
                ("FLOW", Self::FLOW.bits()),
                ("WARNING", Self::WARNING.bits()),
            ],
        );
        (Self::from_bits_retain(bits), rest)
    }
}

impl TraceIoMask {
    /// C `asynTraceIOMaskStringToInt` (asynShellCommands.c:756-783); see
    /// [`TraceMask::from_c_mask_string`].
    pub fn from_c_mask_string(s: &str) -> (TraceIoMask, &str) {
        let (bits, rest) = c_mask_string_to_int(
            s,
            &["TRACEIO_"],
            &[
                ("NODATA", 0),
                ("ASCII", Self::ASCII.bits()),
                ("ESCAPE", Self::ESCAPE.bits()),
                ("HEX", Self::HEX.bits()),
            ],
        );
        (Self::from_bits_retain(bits), rest)
    }
}

impl TraceInfoMask {
    /// C `asynTraceInfoMaskStringToInt` (asynShellCommands.c:822-849); see
    /// [`TraceMask::from_c_mask_string`].
    pub fn from_c_mask_string(s: &str) -> (TraceInfoMask, &str) {
        let (bits, rest) = c_mask_string_to_int(
            s,
            &["TRACEINFO_"],
            &[
                ("TIME", Self::TIME.bits()),
                ("PORT", Self::PORT.bits()),
                ("SOURCE", Self::SOURCE.bits()),
                ("THREAD", Self::THREAD.bits()),
            ],
        );
        (Self::from_bits_retain(bits), rest)
    }
}

/// The loop the three C `asynTrace*MaskStringToInt` share: a `strtol(s, 0)`
/// number, or an optional `ASYN_` then one of `prefixes` then the first of
/// `names` the text starts with (case-insensitive prefix match, C
/// `STARTSWITH`), separated by `|` or `+`. Whitespace is skipped around a
/// name but not after a number, so `"1 | 2"` stops at `" | 2"` as in C.
fn c_mask_string_to_int<'a>(
    s: &'a str,
    prefixes: &[&str],
    names: &[(&str, u32)],
) -> (u32, &'a str) {
    let b = s.as_bytes();
    let starts_with = |i: usize, pat: &str| {
        b.len() - i >= pat.len() && b[i..i + pat.len()].eq_ignore_ascii_case(pat.as_bytes())
    };
    let skip_space = |mut i: usize| {
        while i < b.len() && is_c_space(b[i]) {
            i += 1;
        }
        i
    };
    let mut mask = 0u32;
    let mut i = 0;
    loop {
        if let Some((value, len)) = c_strtol_base0(&b[i..]) {
            // C stores strtol's long in an int.
            mask |= value as i32 as u32;
            i += len;
        } else {
            i = skip_space(i);
            if starts_with(i, "ASYN_") {
                i += "ASYN_".len();
            }
            if let Some(p) = prefixes.iter().find(|p| starts_with(i, p)) {
                i += p.len();
            }
            let Some((name, bit)) = names.iter().find(|(n, _)| starts_with(i, n)) else {
                break;
            };
            mask |= bit;
            i = skip_space(i + name.len());
        }
        if i < b.len() && (b[i] == b'|' || b[i] == b'+') {
            i += 1;
        } else {
            break;
        }
    }
    // Every byte stepped over is ASCII, so `i` is a char boundary.
    (mask, &s[i..])
}

/// C `isspace` in the "C" locale.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C `strtol(s, &end, 0)` on an LP64 `long`: the value and the bytes consumed,
/// or `None` when no conversion was made (`end == s`). Out-of-range values
/// clamp to `LONG_MIN`/`LONG_MAX` as glibc does.
fn c_strtol_base0(b: &[u8]) -> Option<(i64, usize)> {
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let negative = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let radix = if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
        16
    } else if b.get(i) == Some(&b'0') {
        8
    } else {
        10
    };
    let start = i;
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    while let Some(d) = b.get(i).and_then(|&c| (c as char).to_digit(radix)) {
        match magnitude
            .checked_mul(radix as u64)
            .and_then(|m| m.checked_add(d as u64))
        {
            Some(m) => magnitude = m,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    let value = if negative {
        if overflow || magnitude > i64::MIN.unsigned_abs() {
            i64::MIN
        } else {
            (magnitude as i64).wrapping_neg()
        }
    } else if overflow || magnitude > i64::MAX as u64 {
        i64::MAX
    } else {
        magnitude as i64
    };
    Some((value, i))
}

bitflags! {
    /// How to format I/O data — `asynDriver.h:219-222`.
    ///
    /// A **bitfield**, not a choice: C's `traceVprintIOSource` runs one
    /// independent `if` block per set bit (asynManager.c:3171/:3178/:3192), so
    /// `ASCII|HEX` prints the payload twice, in C's order. No bit set
    /// ([`TraceIoMask::NODATA`], `ASYN_TRACEIO_NODATA`) prints no data at all —
    /// just the bare newline of :3215-3221 — and it is what a port starts with.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceIoMask: u32 {
        const ASCII  = 0x0001;
        const ESCAPE = 0x0002;
        const HEX    = 0x0004;
        // C stores whatever int it is given (asynManager.c:2828).
        const _ = !0;
    }
}

impl TraceIoMask {
    /// C `ASYN_TRACEIO_NODATA` (asynDriver.h:219) — the empty mask.
    pub const NODATA: Self = Self::empty();
}

bitflags! {
    /// What metadata to include in trace prefix.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceInfoMask: u32 {
        const TIME   = 0x0001;
        const PORT   = 0x0002;
        const SOURCE = 0x0004;
        const THREAD = 0x0008;
        // C stores whatever int it is given (asynManager.c:2870).
        const _ = !0;
    }
}

/// Output destination for trace messages.
pub enum TraceFile {
    Stderr,
    Stdout,
    /// EPICS errlog sink. C asyn maps the trace file pointer `NULL`
    /// (`fd == 0`, the `<errlog>` token in asynRecord.c:456) to
    /// `errlogPrintf`: every piece C prints is one errlog message, cut at
    /// `maxMsgSize` and handed to errlog's listeners (`Printer`).
    Errlog,
    File(Arc<Mutex<std::fs::File>>),
}

impl TraceFile {
    /// Identity of the sink, the port's analogue of C's `FILE *` value.
    ///
    /// C `getTraceFile` (asynManager.c:2942-2954) returns the raw `FILE *`
    /// — `0` for errlog, `stdout` / `stderr`, or the open file's pointer — and
    /// asynRecord compares it against its remembered `old.traceFd` to decide
    /// whether *another thread* re-pointed the trace file (asynRecord.c:1119).
    /// Pointer identity is the whole content of that test, so the port exposes
    /// the same thing: a stable token per distinct sink. Errlog is `0` as in C;
    /// an open file is its `Arc` address, which cannot collide with the three
    /// standard-sink sentinels.
    pub fn id(&self) -> usize {
        match self {
            TraceFile::Errlog => 0,
            TraceFile::Stdout => 1,
            TraceFile::Stderr => 2,
            TraceFile::File(f) => Arc::as_ptr(f) as usize,
        }
    }

    /// Write a complete line atomically (single write_all call under lock).
    pub fn write_line(&self, line: &str) {
        self.write_bytes(line.as_bytes());
    }

    /// The byte form. A trace I/O line is not text: C's ASCII block is
    /// `fprintf(fp, "%.*s\n", ...)` over the raw device bytes (asynManager.c:
    /// 3173), which carries control bytes and invalid UTF-8 through untouched.
    pub fn write_bytes(&self, line: &[u8]) {
        match self {
            TraceFile::Errlog => errlog_printf_bytes(&[line]),
            TraceFile::Stderr => {
                let _ = StagedStderr.write_all(line);
            }
            TraceFile::Stdout => {
                let _ = std::io::stdout().write_all(line);
            }
            TraceFile::File(f) => {
                if let Ok(mut f) = f.lock() {
                    let _ = f.write_all(line);
                }
            }
        }
    }

    /// One trace print, C's `traceVprintSource` / `traceVprintIOSource` from
    /// the prefix on. A stream sink takes it under its lock, as C's `fprintf`s
    /// into `fp` do: nothing is added, so a line ends where the caller's
    /// format ends it. Staged whole and written once, so another writer's
    /// line cannot land inside it; nothing is allocated below 1 KiB.
    fn print(&self, f: impl FnOnce(&mut Printer<'_, '_>) -> std::fmt::Result) {
        match self {
            TraceFile::Errlog => {
                let _ = f(&mut Printer::Errlog);
            }
            TraceFile::Stderr => stream(&mut StagedStderr, |o| f(&mut Printer::Stream(o))),
            TraceFile::Stdout => stream(&mut std::io::stdout().lock(), |o| {
                f(&mut Printer::Stream(o))
            }),
            TraceFile::File(file) => {
                if let Ok(mut file) = file.lock() {
                    stream(&mut *file, |o| f(&mut Printer::Stream(o)));
                }
            }
        }
    }
}

/// [`TraceFile::print`]'s print to one sink.
fn stream(out: &mut dyn std::io::Write, body: impl FnOnce(&mut TraceOut<'_>) -> std::fmt::Result) {
    let mut staged = TraceOut {
        out,
        line: StagedLine::default(),
    };
    let _ = body(&mut staged);
    let _ = staged.out.write_all(staged.line.bytes());
}

/// Where the pieces of one trace print go. C prints each with its own
/// `fprintf(fp, ...)`, which on a stream run together, or, for errlog
/// (`fp == NULL`), its own `errlogPrintf`: one errlog message each
/// (asynManager.c:2998-3058, :3104-3108, :3165-3221).
enum Printer<'a, 'b> {
    Stream(&'a mut TraceOut<'b>),
    Errlog,
}

impl Printer<'_, '_> {
    /// One C `fprintf` / `errlogPrintf` of text.
    fn text(&mut self, args: std::fmt::Arguments<'_>) -> std::fmt::Result {
        match self {
            Printer::Stream(out) => std::fmt::Write::write_fmt(*out, args),
            Printer::Errlog => {
                errlog_vprintf(args);
                Ok(())
            }
        }
    }

    /// One of device bytes, `parts` run together.
    fn bytes(&mut self, parts: &[&[u8]]) -> std::fmt::Result {
        match self {
            Printer::Stream(out) => parts.iter().try_for_each(|p| out.write_bytes(p)),
            Printer::Errlog => {
                errlog_printf_bytes(parts);
                Ok(())
            }
        }
    }

    /// The caller's message, C's `vfprintf` / `errlogVprintf`. errlog gets it
    /// rendered, so `body` runs once and outside errlog's lock.
    fn body(
        &mut self,
        body: impl FnOnce(&mut TraceOut<'_>) -> std::fmt::Result,
    ) -> std::fmt::Result {
        match self {
            Printer::Stream(out) => body(out),
            Printer::Errlog => {
                let mut line = Vec::new();
                stream(&mut line, body);
                self.bytes(&[&line])
            }
        }
    }
}

/// The trace sink a print body writes into: text through [`std::fmt::Write`]
/// (`write!`), and bytes as they are through [`Self::write_bytes`], as C's
/// `fprintf` writes a `%s` of device bytes. Staged whole for one write.
pub struct TraceOut<'a> {
    out: &'a mut dyn std::io::Write,
    line: StagedLine,
}

impl TraceOut<'_> {
    /// Write `s` untouched: invalid UTF-8 and control bytes included.
    pub fn write_bytes(&mut self, s: &[u8]) -> std::fmt::Result {
        self.line.push_bytes(s);
        Ok(())
    }
}

impl std::fmt::Write for TraceOut<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.write_bytes(s.as_bytes())
    }
}

impl Default for TraceFile {
    fn default() -> Self {
        TraceFile::Stderr
    }
}

/// The effective trace configuration for one `(port, addr)` — see
/// [`TraceManager::snapshot`].
#[derive(Clone, Copy, Debug)]
pub struct TraceSnapshot {
    pub trace_mask: TraceMask,
    pub io_mask: TraceIoMask,
    pub info_mask: TraceInfoMask,
    pub io_truncate_size: usize,
    /// Identity of the trace sink — see [`TraceFile::id`].
    pub file_id: usize,
}

/// C `DEFAULT_TRACE_BUFFER_SIZE` (asynManager.c:47) — the size `tracePvtInit`
/// (:453) allocates `tracePvt.traceBuffer` with. That buffer is the destination
/// `epicsStrSnPrintEscaped` writes the ESCAPE form into on the errlog branch
/// (:3184), so it is the bound that truncates an errlog trace line.
const DEFAULT_TRACE_BUFFER_SIZE: usize = 80;

/// Per-port (or global) trace configuration.
pub struct TraceConfig {
    pub trace_mask: TraceMask,
    pub trace_io_mask: TraceIoMask,
    pub trace_info_mask: TraceInfoMask,
    pub io_truncate_size: usize,
    /// C `tracePvt.traceBufferSize`. Starts at `DEFAULT_TRACE_BUFFER_SIZE` and
    /// is grown — never shrunk — by `setTraceIOTruncateSize` when the new
    /// truncate size exceeds it (asynManager.c:2963-2979).
    pub trace_buffer_size: usize,
    pub file: TraceFile,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            // C parity: `tracePvtInit` (asynManager.c:456) initializes
            // every port's mask to `ASYN_TRACE_ERROR` only. WARNING is
            // OFF by default and must be enabled via `asynSetTraceMask`;
            // shipping it ON makes ASYN_TRACE_WARNING diagnostics (e.g.
            // NDPluginDriver's "no input array cached") spam stderr at
            // iocInit, which C keeps silent.
            trace_mask: TraceMask::ERROR,
            // C `tracePvtInit` (asynManager.c:451-461) never assigns
            // `traceIOMask`, and `callocMustSucceed` zeroed the `tracePvt` —
            // so a port starts with NO I/O-data bit set, and `asynPrintIO`
            // prints its message and a bare newline until `asynSetTraceIOMask`
            // turns a form on (:3215-3221). ASCII is a value an operator asks
            // for, not one a port is born with.
            trace_io_mask: TraceIoMask::NODATA,
            // C `tracePvtInit` (asynManager.c:457) sets
            // `traceInfoMask = ASYN_TRACEINFO_TIME` and nothing else, so a port
            // is born with only the TIME bit: `monitorStatus` reads back
            // `TINM=1`, `TINB0(time)=On`, `TINB1(port)=Off`. PORT is a bit an
            // operator turns on, not one a port starts with.
            trace_info_mask: TraceInfoMask::TIME,
            io_truncate_size: 80,
            trace_buffer_size: DEFAULT_TRACE_BUFFER_SIZE,
            file: TraceFile::default(),
        }
    }
}

/// C `setTraceIOTruncateSize`'s `asynError`: the `calloc` that grows
/// `traceBuffer` to the new size failed, and C returns before it writes the
/// size or announces anything (asynManager.c:2963-2975). Every negative
/// `epicsInt32` size lands here, sign-extended past what any allocation can
/// hold; `isize::MAX` is that bound in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncateSizeRefused(pub usize);

impl TruncateSizeRefused {
    fn check(size: usize) -> Result<(), Self> {
        match std::alloc::Layout::array::<u8>(size) {
            Ok(_) => Ok(()),
            Err(_) => Err(Self(size)),
        }
    }
}

/// One port's `dpCommon` trace state — C's `port` struct as the trace
/// facility sees it (asynManager.c:512-531).
///
/// `pport->dpc.trace` and one `pdevice->dpc.trace` per address, plus the
/// `ASYN_MULTIDEVICE` attribute, because that attribute is what decides
/// whether an address names a device slot at all: `locateDevice` returns
/// NULL for a single-device port (:576), so `connectDevice` leaves
/// `puserPvt->pdevice` NULL there (:1350-1353) and both `findDpCommon`
/// (:538-545) and every `setTrace*` land on the port.
struct PortTrace {
    /// C `pport->dpc.trace`.
    port: TraceConfig,
    /// C `pdevice->dpc.trace`, one entry per address a caller has scoped a
    /// setting to. An address with no entry reads the port's slot, which
    /// after any port-scoped write holds the pushed-down value — the same
    /// value C's push-down wrote into the device's own slot.
    devices: HashMap<i32, TraceConfig>,
    /// C `pport->attributes & ASYN_MULTIDEVICE`.
    multi_device: bool,
    /// The port's announcement owner, once a port of this name is bound to
    /// these services: C's `setTrace*` raise their exception on the
    /// `dpCommon` written (`announceExceptionOccurred`, asynManager.c:2804),
    /// so its exception users hear it, not only the IOC-wide list.
    announcer: Option<crate::port::ExceptionAnnouncer>,
}

impl PortTrace {
    fn new(multi_device: bool) -> Self {
        Self {
            // C `dpCommonInit` → `tracePvtInit` (asynManager.c:451-461): a
            // port is born with the defaults, NOT with a copy of
            // `pasynBase->trace`.
            port: TraceConfig::default(),
            devices: HashMap::new(),
            multi_device,
            announcer: None,
        }
    }
}

/// C `locateDevice` (asynManager.c:571-590) and the `findDpCommon` gate it
/// feeds (:538-545), as one predicate: an address names a device slot only on
/// an `ASYN_MULTIDEVICE` port, and only when it is non-negative.
///
/// One predicate for reads and writes both, because C applies it on both
/// sides through the same `puserPvt->pdevice`: `asynSetTraceMask P 5 0x3f`
/// on a single-device port is `asynSetTraceMask P -1 0x3f`.
fn device_slot(multi_device: bool, addr: Option<i32>) -> Option<i32> {
    crate::port::dp_common_key(multi_device, addr.unwrap_or(-1))
}

/// Global trace manager holding C's `dpCommon` tree.
///
/// # Invariant
///
/// **The configuration a `(port, addr)` reads is exactly one slot, chosen by
/// `device_slot`, and every port-scoped write pushes down into every device
/// slot of that port before writing the port's own.** So no device slot can
/// shadow a later port-level set, and no read walks past the port.
///
/// `TraceManager::with_dp_common` is the only resolver and
/// `TraceManager::write_scoped` the only mutator; the setters below are
/// thin wrappers that name which field to write.
pub struct TraceManager {
    /// C `pasynBase->trace` (asynManager.c:505), written by the
    /// `pasynUser == NULL` arm of every `setTrace*`. `findTracePvt` reaches
    /// it only when `findDpCommon` returns NULL — that is, for a user with no
    /// port (:548-553) — so **no port-attached user reads it** and no output
    /// path here does either. Kept because C keeps it and because
    /// `get_trace_*(None)` answers the `asynSetTraceMask ""` readback from it.
    global_config: Mutex<TraceConfig>,
    /// C `pasynBase->asynPortList`, from the trace facility's point of view.
    ports: Mutex<HashMap<String, PortTrace>>,
    /// Optional sink for trace-mutator exceptions. C asyn fires
    /// `asynExceptionTrace{Mask,IOMask,InfoMask,File,IOTruncateSize}`
    /// from every `setTrace*` (asynManager.c:2804/2846/2888/2937/2981).
    /// `Mutex` rather than `OnceCell` so a manager can install the sink
    /// after construction (PortManager builds both objects, then wires
    /// the trace sink after).
    exception_sink: Mutex<Option<Arc<ExceptionManager>>>,
    /// Every trace-mask bit any slot has held. It only grows, so a mask
    /// outside it cannot print anywhere and the emitters return without the
    /// `ports` lock, as C's `asynPrint` tests the mask without one.
    masks_set: AtomicU32,
}

impl TraceManager {
    pub fn new() -> Self {
        Self {
            global_config: Mutex::new(TraceConfig::default()),
            ports: Mutex::new(HashMap::new()),
            exception_sink: Mutex::new(None),
            masks_set: AtomicU32::new(TraceConfig::default().trace_mask.bits()),
        }
    }

    fn may_trace(&self, mask: TraceMask) -> bool {
        self.masks_set.load(Ordering::Relaxed) & mask.bits() != 0
    }

    /// Tell the manager a port exists and whether it is `ASYN_MULTIDEVICE` —
    /// C `registerPort` (asynManager.c:2047-2097), which builds the port's
    /// `dpCommon` and records its attributes in the same call that claims the
    /// name.
    ///
    /// Called from [`crate::registry::PortClaim::publish`], the one site
    /// that publishes a port name in this process, so the attribute cannot go
    /// unrecorded for a port that exists. A port whose registration this
    /// manager never saw is treated as single-device, which is C's default
    /// (`attributes` is whatever the driver passed, and `ASYN_MULTIDEVICE` is
    /// opt-in).
    ///
    /// Re-registering a name keeps the slots already configured and only
    /// updates the attribute: an `st.cmd` may set a trace mask on a port
    /// before anything registers it.
    pub fn register_port(&self, port: &str, multi_device: bool) {
        if let Ok(mut ports) = self.ports.lock() {
            ports
                .entry(port.to_string())
                .or_insert_with(|| PortTrace::new(multi_device))
                .multi_device = multi_device;
        }
    }

    /// Route this port's trace exceptions through `announcer`
    /// ([`crate::port_handle::PortHandle::new`], every port's creation).
    pub(crate) fn bind_announcer(&self, port: &str, announcer: crate::port::ExceptionAnnouncer) {
        if let Ok(mut ports) = self.ports.lock() {
            ports
                .entry(port.to_string())
                .or_insert_with(|| PortTrace::new(false))
                .announcer = Some(announcer);
        }
    }

    /// Raise a trace exception on `port`'s `dpCommon` for `addr`: through the
    /// port's announcer when one is bound, else on the IOC-wide list alone.
    fn raise(
        &self,
        port: &str,
        announcer: Option<crate::port::ExceptionAnnouncer>,
        exception: AsynException,
        addr: i32,
    ) {
        if let Some(announcer) = announcer {
            announcer.announce(exception, addr);
            return;
        }
        let sink = self.exception_sink.lock().ok().and_then(|g| g.clone());
        if let Some(sink) = sink {
            sink.announce(&ExceptionEvent {
                port_name: port,
                exception,
                addr,
            });
        }
    }

    /// C `connectDevice`'s `locateDevice(pport, addr, TRUE)`
    /// (asynManager.c:571-590): give `addr` its own slot, born with
    /// `tracePvtInit`'s defaults, so later port-scoped mask writes push down
    /// into it while a port-scoped trace file does not reach it. No-op where
    /// the address names no device.
    pub fn connect_device(&self, port: &str, addr: i32) {
        if let Ok(mut ports) = self.ports.lock() {
            if let Some(pt) = ports.get_mut(port) {
                if let Some(a) = device_slot(pt.multi_device, Some(addr)) {
                    pt.devices.entry(a).or_default();
                }
            }
        }
    }

    /// Install the exception sink used by every `set_trace_*` mutator.
    /// Mirrors C asyn where `setTraceMask` / `setTraceIOMask` /
    /// `setTraceInfoMask` / `setTraceFile` / `setTraceIOTruncateSize`
    /// each call `announceExceptionOccurred`. Callers that want trace
    /// listeners (asynShellCommands UI, asynRecord, monitor relays)
    /// to react to trace re-configuration must install the sink.
    pub fn set_exception_sink(&self, sink: Arc<ExceptionManager>) {
        if let Ok(mut slot) = self.exception_sink.lock() {
            *slot = Some(sink);
        }
    }

    /// Return the installed exception sink, if any. C asyn delivers
    /// `setTrace*` reconfiguration to listeners through
    /// `exceptionCallbackAdd` (asynManager.c); asynRecord retrieves the
    /// sink here to register the trace-status refresh callback that C
    /// installs in `connectDevice` (asynRecord.c:1269).
    pub fn exception_manager(&self) -> Option<Arc<ExceptionManager>> {
        self.exception_sink.lock().ok().and_then(|g| g.clone())
    }

    /// Fire a trace exception to the registered sink, if any.
    /// `port = None` corresponds to a global change.
    fn announce(&self, port: Option<&str>, exception: AsynException) {
        let sink = match self.exception_sink.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        if let Some(sink) = sink {
            sink.announce(&ExceptionEvent {
                port_name: port.unwrap_or(""),
                exception,
                addr: -1,
            });
        }
    }

    /// Check if a trace level is enabled for a port (optionally device).
    ///
    /// `mask` should be a single trace level (e.g. `TraceMask::ERROR`), not a
    /// combination. In debug builds, passing a multi-bit mask triggers a
    /// `debug_assert` failure.
    pub fn is_enabled(&self, port: &str, mask: TraceMask) -> bool {
        debug_assert!(
            mask.bits().is_power_of_two(),
            "is_enabled expects a single trace level, got {:?}",
            mask
        );
        self.may_trace(mask)
            && self
                .with_dp_common(port, None, |cfg| cfg.trace_mask.intersects(mask))
                .unwrap_or(false)
    }

    /// Check if a trace level is enabled for a specific device address —
    /// the same `findDpCommon` resolution the emit path uses.
    pub fn is_enabled_device(&self, port: &str, addr: i32, mask: TraceMask) -> bool {
        self.may_trace(mask)
            && self
                .with_dp_common(port, Some(addr), |cfg| cfg.trace_mask.intersects(mask))
                .unwrap_or(false)
    }

    /// Run `f` with the one `dpCommon` trace slot C's `findDpCommon`
    /// (asynManager.c:538-545) selects for `(port, addr)`.
    ///
    /// There is no chain: C picks one whole struct and reads it. The only
    /// step this takes that C does not is falling from an unconfigured device
    /// to the port's slot, and that is not a fallback in the old sense — a
    /// port-scoped write pushes its value into every device slot, so the port
    /// slot holds exactly what C's push-down had written into the device's.
    ///
    /// A name the manager has never been told about carries C's born-with
    /// `tracePvtInit` defaults (:451-461), never `pasynBase->trace`:
    /// `findTracePvt` reaches the global only for a user with no port at all
    /// (:548-553), which no output path here can produce.
    pub(crate) fn with_dp_common<R, F>(&self, port: &str, addr: Option<i32>, f: F) -> Option<R>
    where
        F: FnOnce(&TraceConfig) -> R,
    {
        self.with_dp_common_addr(port, addr, |cfg, _| f(cfg))
    }

    /// [`Self::with_dp_common`] also handing `f` the address C's `getAddr`
    /// (asynManager.c:2006-2010) reports for the user, which `printPort`
    /// prints: -1 on a port without `ASYN_MULTIDEVICE`, whatever `addr` was.
    fn with_dp_common_addr<R, F>(&self, port: &str, addr: Option<i32>, f: F) -> Option<R>
    where
        F: FnOnce(&TraceConfig, Option<i32>) -> R,
    {
        let ports = self.ports.lock().ok()?;
        let Some(pt) = ports.get(port) else {
            return Some(f(&TraceConfig::default(), addr));
        };
        match device_slot(pt.multi_device, addr) {
            Some(a) => Some(f(pt.devices.get(&a).unwrap_or(&pt.port), Some(a))),
            None => Some(f(&pt.port, None)),
        }
    }

    /// The one mutator. C `setTraceMask` (asynManager.c:2788-2816) and its
    /// three siblings share this shape exactly:
    ///
    /// - a **device-scoped** write touches that device's slot alone and
    ///   announces once for it (:2802-2805);
    /// - a **port-scoped** write **pushes down** — every device slot first,
    ///   announcing per device, then the port's own with a port announce
    ///   (:2806-2815).
    ///
    /// The push-down is what makes `asynSetTraceMask P -1 0x1` after
    /// `asynSetTraceMask P 1 0x3f` quiet device 1. Announces fire after the
    /// map lock is released, because a listener may read back through the
    /// manager.
    fn write_scoped<F>(&self, port: &str, addr: Option<i32>, exception: AsynException, mut apply: F)
    where
        F: FnMut(&mut TraceConfig),
    {
        let mut announced: Vec<i32> = Vec::new();
        let mut announcer = None;
        if let Ok(mut ports) = self.ports.lock() {
            let pt = ports
                .entry(port.to_string())
                .or_insert_with(|| PortTrace::new(false));
            announcer = pt.announcer.clone();
            match device_slot(pt.multi_device, addr) {
                Some(a) => {
                    apply(pt.devices.entry(a).or_insert_with(TraceConfig::default));
                    announced.push(a);
                }
                None => {
                    let mut addrs: Vec<i32> = pt.devices.keys().copied().collect();
                    addrs.sort_unstable();
                    for a in &addrs {
                        if let Some(cfg) = pt.devices.get_mut(a) {
                            apply(cfg);
                        }
                    }
                    apply(&mut pt.port);
                    announced.extend(addrs);
                    // C `announceExceptionOccurred(pport, NULL, ...)` — the
                    // port itself, which this crate renders as addr -1.
                    announced.push(-1);
                }
            }
        }
        for a in announced {
            self.raise(port, announcer.clone(), exception, a);
        }
    }

    /// The single-slot mutator, for the two setters that are **not** a
    /// push-down.
    ///
    /// `setTraceFile` (asynManager.c:2912-2940) and `setTraceIOTruncateSize`
    /// (:2957-2984) open on `findTracePvt(puserPvt)` and write that one
    /// `tracePvt`, where the mask family walks the device list instead. So a
    /// port-scoped trace file does not follow a device that has its own, and
    /// C does not intend it to: the mask family's push-down is written out
    /// device by device precisely because these two are not.
    ///
    /// The slot is chosen by the same [`device_slot`] predicate, so a device
    /// address on a single-device port lands on the port here too.
    fn write_one_slot<F>(
        &self,
        port: &str,
        addr: Option<i32>,
        exception: Option<AsynException>,
        apply: F,
    ) where
        F: FnOnce(&mut TraceConfig),
    {
        let mut announced_at = None;
        let mut announcer = None;
        if let Ok(mut ports) = self.ports.lock() {
            let pt = ports
                .entry(port.to_string())
                .or_insert_with(|| PortTrace::new(false));
            announcer = pt.announcer.clone();
            match device_slot(pt.multi_device, addr) {
                Some(a) => {
                    apply(pt.devices.entry(a).or_insert_with(TraceConfig::default));
                    announced_at = Some(a);
                }
                None => {
                    apply(&mut pt.port);
                    announced_at = Some(-1);
                }
            }
        }
        let (Some(exception), Some(a)) = (exception, announced_at) else {
            return;
        };
        self.raise(port, announcer, exception, a);
    }

    /// Set trace configuration for a specific device address.
    ///
    /// C parity: asynManager.c:2802-2805 fires `asynExceptionTraceMask`
    /// for the per-device mutation case. On a single-device port the address
    /// names no device (`locateDevice`, :576) and this is the port-scoped
    /// write, exactly as it is in C.
    pub fn set_device_trace_mask(&self, port: &str, addr: i32, mask: TraceMask) {
        self.masks_set.fetch_or(mask.bits(), Ordering::Relaxed);
        self.write_scoped(port, Some(addr), AsynException::TraceMask, |cfg| {
            cfg.trace_mask = mask
        });
    }

    /// Output a trace message (port-level resolution).
    ///
    /// Equivalent to [`Self::output_device`] with `addr = None` and the
    /// reason 0 that C's `pasynUserSelf` carries — a port's own diagnostic
    /// user is created by `createAsynUser(0,0)` and never assigned one.
    #[track_caller]
    pub fn output(&self, port: &str, mask: TraceMask, msg: &str) {
        self.output_device(port, None, 0, mask, msg);
    }

    /// C `asynPrint` of `msg`, resolving config device → port → global
    /// through `findTracePvt`, which walks `pasynUser`'s device-pvt first when
    /// `pdevice != NULL`. The source is the caller's, as for [`Self::print`].
    ///
    /// `reason` is the user's `pasynUser->reason`, printed in the
    /// `[port,addr,reason]` triple — see `write_prefix`.
    #[track_caller]
    pub fn output_device(
        &self,
        port: &str,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        msg: &str,
    ) {
        let caller = std::panic::Location::caller();
        self.output_device_with_source(port, addr, reason, mask, caller.file(), caller.line(), msg);
    }

    /// C `asynPrint` of one line on the rung C's `findTracePvt` picks — `port:
    /// None` is a user on no port, which prints on the global rung
    /// (asynManager.c:548-553). C's `traceVprintSource` `fprintf`s straight
    /// into the trace file (asynManager.c:3090-3115) and allocates nothing, so
    /// a caller holding a record's lock does not either; see
    /// `TraceFile::print`.
    ///
    /// The SOURCE component names the caller, because C's `asynPrint` macro
    /// passes `__FILE__`/`__LINE__` to `printSource` (asynDriver.h:296-306).
    /// Only C's `pasynTrace->vprint` function prints `[:0]`; a port of one
    /// says so with [`Self::print_source`] and `("", 0)`.
    #[track_caller]
    pub fn print(
        &self,
        port: Option<&str>,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        args: std::fmt::Arguments<'_>,
    ) {
        let caller = std::panic::Location::caller();
        self.print_source(
            port,
            addr,
            reason,
            mask,
            (caller.file(), caller.line()),
            args,
        );
    }

    /// [`Self::print`] with an explicit `__FILE__`/`__LINE__`, which the
    /// `ASYN_TRACEINFO_SOURCE` prefix prints (asynManager.c:3090-3115).
    pub fn print_source(
        &self,
        port: Option<&str>,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        source: (&str, u32),
        args: std::fmt::Arguments<'_>,
    ) {
        self.print_with(port, addr, reason, mask, source, |out| {
            std::fmt::Write::write_fmt(out, args)
        });
    }

    /// [`Self::print_source`] with a body that writes the message itself, so
    /// it can carry raw bytes ([`TraceOut::write_bytes`]). Runs only when
    /// `mask` is enabled.
    pub fn print_with(
        &self,
        port: Option<&str>,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        source: (&str, u32),
        body: impl FnOnce(&mut TraceOut<'_>) -> std::fmt::Result,
    ) {
        if !self.may_trace(mask) {
            return;
        }
        let emit = |cfg: &TraceConfig, addr: Option<i32>| {
            if !cfg.trace_mask.intersects(mask) {
                return;
            }
            cfg.file.print(|p| {
                write_prefix(p, port, addr, reason, source, cfg)?;
                p.body(body)
            });
        };
        match port {
            Some(port) => {
                self.with_dp_common_addr(port, addr, emit);
            }
            None => {
                if let Ok(cfg) = self.global_config.lock() {
                    emit(&cfg, addr);
                }
            }
        }
    }

    /// Output a trace message with source file/line info (C parity: __FILE__/__LINE__).
    pub fn output_with_source(
        &self,
        port: &str,
        mask: TraceMask,
        file: &str,
        line: u32,
        msg: &str,
    ) {
        self.output_device_with_source(port, None, 0, mask, file, line, msg);
    }

    /// Device-aware variant — resolves config device → port → global.
    /// C parity: `tracePrintSource` (asynManager.c:3074-3082).
    #[allow(clippy::too_many_arguments)]
    pub fn output_device_with_source(
        &self,
        port: &str,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        file: &str,
        line: u32,
        msg: &str,
    ) {
        if !self.may_trace(mask) {
            return;
        }
        self.with_dp_common_addr(port, addr, |cfg, addr| {
            if !cfg.trace_mask.intersects(mask) {
                return;
            }
            cfg.file.print(|p| {
                write_prefix(p, Some(port), addr, reason, (file, line), cfg)?;
                p.text(format_args!("{msg}"))
            });
        });
    }

    /// Output I/O data with formatting according to TraceIoMask.
    ///
    /// `file`/`line` are the caller's `__FILE__`/`__LINE__`: C's `asynPrintIO`
    /// is a macro that captures them (asynDriver.h:296-299) and hands them to
    /// `printIOSource`, so the SOURCE component is available on this path
    /// exactly as it is on `asynPrint`'s.
    pub fn output_io(
        &self,
        port: &str,
        mask: TraceMask,
        data: &[u8],
        label: &str,
        file: &str,
        line: u32,
    ) {
        self.output_device_io(port, None, 0, mask, data, label, file, line);
    }

    /// Device-aware variant — resolves config device → port → global.
    /// C parity: `tracePrintIO` (asynManager.c:3115-3124). The `addr`
    /// participates in both config resolution AND the `[port,addr,reason]`
    /// `printPort` prefix.
    ///
    /// `label` is C's `vfprintf(fp, pformat, pvar)` output, printed as given,
    /// and the *data section* follows it, one block per enabled
    /// [`TraceIoMask`] bit (`print_io_data`).
    #[allow(clippy::too_many_arguments)]
    pub fn output_device_io(
        &self,
        port: &str,
        addr: Option<i32>,
        reason: i32,
        mask: TraceMask,
        data: &[u8],
        label: &str,
        file: &str,
        line: u32,
    ) {
        if !self.may_trace(mask) {
            return;
        }
        self.with_dp_common_addr(port, addr, |cfg, addr| {
            if !cfg.trace_mask.intersects(mask) {
                return;
            }
            cfg.file.print(|p| {
                write_prefix(p, Some(port), addr, reason, (file, line), cfg)?;
                p.text(format_args!("{label}"))?;
                print_io_data(p, data, cfg)
            });
        });
    }

    // --- Configuration mutators ---

    /// C parity: asynManager.c:2804/2814 fires `asynExceptionTraceMask`
    /// after the mutation, for either the per-port path or the
    /// "no pasynUser → global" path.
    pub fn set_trace_mask(&self, port: Option<&str>, mask: TraceMask) {
        self.masks_set.fetch_or(mask.bits(), Ordering::Relaxed);
        match port {
            Some(name) => {
                self.write_scoped(name, None, AsynException::TraceMask, |cfg| {
                    cfg.trace_mask = mask
                });
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_mask = mask;
                }
                self.announce(None, AsynException::TraceMask);
            }
        }
    }

    /// C parity: asynManager.c:2846/2856 fires `asynExceptionTraceIOMask`.
    pub fn set_trace_io_mask(&self, port: Option<&str>, mask: TraceIoMask) {
        match port {
            Some(name) => {
                self.write_scoped(name, None, AsynException::TraceIoMask, |cfg| {
                    cfg.trace_io_mask = mask
                });
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_io_mask = mask;
                }
                self.announce(None, AsynException::TraceIoMask);
            }
        }
    }

    /// Per-device variant of [`Self::set_trace_io_mask`].
    ///
    /// C parity: `setTraceIOMask` (asynManager.c:2828-2860) writes the
    /// IO mask into `pdevice->dpc.trace.traceIOMask` when the asynUser
    /// is connected with `addr >= 0` (`pdevice != NULL`) and announces
    /// per-device. The IO/InfoMask/File analogues of
    /// [`Self::set_device_trace_mask`] must mirror that routing so the
    /// `asynSetTraceIOMask MYPORT N "ESCAPE"` iocsh call (and any
    /// programmatic device-scoped trace setup) actually overrides the
    /// `(port, addr)` slot resolved by `Self::with_dp_common`.
    pub fn set_device_trace_io_mask(&self, port: &str, addr: i32, mask: TraceIoMask) {
        self.write_scoped(port, Some(addr), AsynException::TraceIoMask, |cfg| {
            cfg.trace_io_mask = mask
        });
    }

    /// C parity: asynManager.c:2888/2898 fires `asynExceptionTraceInfoMask`.
    pub fn set_trace_info_mask(&self, port: Option<&str>, mask: TraceInfoMask) {
        match port {
            Some(name) => {
                self.write_scoped(name, None, AsynException::TraceInfoMask, |cfg| {
                    cfg.trace_info_mask = mask
                });
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_info_mask = mask;
                }
                self.announce(None, AsynException::TraceInfoMask);
            }
        }
    }

    /// Per-device variant of [`Self::set_trace_info_mask`].
    ///
    /// C parity: `setTraceInfoMask` (asynManager.c:2870-2902) writes
    /// the info mask into `pdevice->dpc.trace.traceInfoMask` when
    /// `pdevice != NULL` and announces per-device.
    pub fn set_device_trace_info_mask(&self, port: &str, addr: i32, mask: TraceInfoMask) {
        self.write_scoped(port, Some(addr), AsynException::TraceInfoMask, |cfg| {
            cfg.trace_info_mask = mask
        });
    }

    /// C parity: asynManager.c:2937 fires `asynExceptionTraceFile`
    /// after the mutation completes (the C path always implies a
    /// port-scoped pasynUser; we mirror that by only firing when a
    /// port name is supplied).
    pub fn set_trace_file(&self, port: Option<&str>, file: TraceFile) {
        match port {
            Some(name) => self.write_one_slot(name, None, Some(AsynException::TraceFile), |cfg| {
                cfg.file = file
            }),
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.file = file;
                }
                // C `setTraceFile` announces only when `puserPvt->pport` is
                // non-null (asynManager.c:2937).
            }
        }
    }

    /// Per-device variant of [`Self::set_trace_file`].
    ///
    /// C parity: `setTraceFile` (asynManager.c:2912-2940) resolves
    /// `findTracePvt(puserPvt)`, which returns the device-specific
    /// `dpCommon` when the asynUser carries a `pdevice`; writes the
    /// new FP there; fires `asynExceptionTraceFile`.
    pub fn set_device_trace_file(&self, port: &str, addr: i32, file: TraceFile) {
        self.write_one_slot(port, Some(addr), Some(AsynException::TraceFile), |cfg| {
            cfg.file = file
        });
    }

    /// C parity: asynManager.c:2981 fires
    /// `asynExceptionTraceIOTruncateSize` after the mutation.
    ///
    /// C also re-allocates `traceBuffer` to `size` when the new truncate size
    /// exceeds the current buffer (asynManager.c:2963-2979) — see
    /// [`TraceConfig::trace_buffer_size`], which bounds the errlog ESCAPE form.
    /// A size that buffer cannot be grown to is refused: [`TruncateSizeRefused`].
    pub fn set_io_truncate_size(
        &self,
        port: Option<&str>,
        size: usize,
    ) -> Result<(), TruncateSizeRefused> {
        TruncateSizeRefused::check(size)?;
        match port {
            Some(name) => self.write_one_slot(
                name,
                None,
                Some(AsynException::TraceIoTruncateSize),
                |cfg| {
                    cfg.io_truncate_size = size;
                    cfg.trace_buffer_size = cfg.trace_buffer_size.max(size);
                },
            ),
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.io_truncate_size = size;
                    cfg.trace_buffer_size = cfg.trace_buffer_size.max(size);
                }
                // C announces only when `puserPvt->pport` is non-null
                // (asynManager.c:2981).
            }
        }
        Ok(())
    }

    /// Per-device variant of [`Self::set_io_truncate_size`].
    ///
    /// C parity: `setTraceIOTruncateSize` (asynManager.c:2957-2984) writes
    /// the truncate size into the device `dpCommon` resolved by
    /// `findTracePvt` when the asynUser carries a device, and announces
    /// `asynExceptionTraceIOTruncateSize` per device.
    pub fn set_device_io_truncate_size(
        &self,
        port: &str,
        addr: i32,
        size: usize,
    ) -> Result<(), TruncateSizeRefused> {
        TruncateSizeRefused::check(size)?;
        self.write_one_slot(
            port,
            Some(addr),
            Some(AsynException::TraceIoTruncateSize),
            |cfg| {
                cfg.io_truncate_size = size;
                cfg.trace_buffer_size = cfg.trace_buffer_size.max(size);
            },
        );
        Ok(())
    }

    pub fn get_trace_mask(&self, port: Option<&str>) -> TraceMask {
        match port {
            Some(name) => self
                .with_dp_common(name, None, |cfg| cfg.trace_mask)
                // C parity: default port mask is ERROR-only
                // (asynManager.c:456); keep the poisoned-lock fallback in
                // sync with TraceConfig::default.
                .unwrap_or(TraceMask::ERROR),
            None => self
                .global_config
                .lock()
                .map(|c| c.trace_mask)
                .unwrap_or(TraceMask::ERROR),
        }
    }

    pub fn get_trace_io_mask(&self, port: Option<&str>) -> TraceIoMask {
        match port {
            Some(name) => self
                .with_dp_common(name, None, |cfg| cfg.trace_io_mask)
                .unwrap_or(TraceIoMask::NODATA),
            None => self
                .global_config
                .lock()
                .map(|c| c.trace_io_mask)
                .unwrap_or(TraceIoMask::NODATA),
        }
    }

    /// Every value C's `monitorStatus` reads back from the trace facility, for
    /// one `(port, addr)`, resolved once.
    ///
    /// C reads them through `pasynTrace->getTraceMask/getTraceIOMask/
    /// getTraceInfoMask/getTraceIOTruncateSize/getTraceFile` on the record's
    /// `pasynUser` (asynRecord.c:1066-1101). All five resolve through the same
    /// `findTracePvt` chain — device, else port, else global
    /// (asynManager.c:548-553) — which is also the chain the record's `setTrace*`
    /// writes target. One snapshot keeps read and write on the same rung: a
    /// per-device write read back at port level would snap the record's field
    /// back to the port's value on the next refresh. `port: None` is a user
    /// connected to no port, which reads the global rung, as the setters' `None`
    /// writes it.
    pub fn snapshot(&self, port: Option<&str>, addr: Option<i32>) -> TraceSnapshot {
        let of = |cfg: &TraceConfig| TraceSnapshot {
            trace_mask: cfg.trace_mask,
            io_mask: cfg.trace_io_mask,
            info_mask: cfg.trace_info_mask,
            io_truncate_size: cfg.io_truncate_size,
            file_id: cfg.file.id(),
        };
        match port {
            Some(port) => self.with_dp_common(port, addr, of),
            None => self.global_config.lock().ok().map(|cfg| of(&cfg)),
        }
        .unwrap_or_else(|| of(&TraceConfig::default()))
    }

    /// C parity: `getTraceInfoMask` (asynManager.c) — the per-port trace
    /// info mask, falling back to the global default. Read by
    /// `monitorStatus` (asynRecord.c:1079) to refresh `TINM`/`TINB0..3`.
    pub fn get_trace_info_mask(&self, port: Option<&str>) -> TraceInfoMask {
        match port {
            Some(name) => self
                .with_dp_common(name, None, |cfg| cfg.trace_info_mask)
                // Matches `TraceConfig::default` — a port born with only the
                // TIME bit (C `tracePvtInit`, asynManager.c:457).
                .unwrap_or(TraceInfoMask::TIME),
            None => self
                .global_config
                .lock()
                .map(|c| c.trace_info_mask)
                .unwrap_or(TraceInfoMask::TIME),
        }
    }
}

impl Default for TraceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// C `asynStripPath` (asynManager.c:481-489) — the basename after the last
/// `/`, and after the last `\` as well on the Windows/Cygwin builds. Rust's
/// `file!()` is a full workspace-relative path, so without this the SOURCE
/// component prints a path where C prints a bare file name.
fn strip_path(file: &str) -> &str {
    let after_slash = match file.rfind('/') {
        Some(i) => &file[i + 1..],
        None => file,
    };
    #[cfg(windows)]
    {
        match after_slash.rfind('\\') {
            Some(i) => &after_slash[i + 1..],
            None => after_slash,
        }
    }
    #[cfg(not(windows))]
    after_slash
}

/// The four `ASYN_TRACEINFO_*` components of one trace line, in the order
/// `traceVprintSource` (asynManager.c:3099-3106) and `traceVprintIOSource`
/// (:3161-3164) test their bits: TIME, PORT, SOURCE, THREAD. Each is an
/// independent `if` writing its own trailing space, so an unset bit
/// contributes nothing and an empty info mask yields no prefix at all.
///
/// `reason` is `pasynUser->reason` — the asyn *parameter* index the user
/// carries, not the trace mask this line is printed under. C's `printPort`
/// (:3030-3048) writes the two side by side in one `[port,addr,reason]`
/// triple, and `getAddr` (:2006) yields **-1** for a user that was never
/// connected to a device, which is what `addr = None` means here.
///
/// `(file, line)` is `("", 0)` only for a port of C's `vprint`/`printIO`
/// functions: `traceVprint` (:3085-3088) and `traceVprintIO` (:3138-3143)
/// call the `*Source` form with exactly that, so C prints `[:0] ` there. The
/// `asynPrint`/`asynPrintIO` macros pass `__FILE__`/`__LINE__`
/// (asynDriver.h:296-320).
///
/// The mask itself is deliberately absent: C prints no severity token, and
/// the `ERROR`/`FLOW`/`IO_DRIVER` label this used to append had no C source.
///
/// `port: None` is a user on no port, for which C's `printPort` prints
/// nothing (asynManager.c:3036-3038). Allocates nothing of its own.
fn write_prefix(
    out: &mut Printer<'_, '_>,
    port: Option<&str>,
    addr: Option<i32>,
    reason: i32,
    source: (&str, u32),
    cfg: &TraceConfig,
) -> std::fmt::Result {
    if cfg.trace_info_mask.contains(TraceInfoMask::TIME) {
        // C `printTime` (asynManager.c:3009-3028): `epicsTimeToStrftime` with
        // `"%Y/%m/%d %H:%M:%S.%03f"`, which formats **local** time. Written
        // field by field: chrono's `format` builds the offset name and the
        // text in `String`s.
        use chrono::{Datelike, Timelike};
        let now = chrono::Local::now();
        out.text(format_args!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}.{:03} ",
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            now.nanosecond() % 1_000_000_000 / 1_000_000
        ))?;
    }

    if let (true, Some(port)) = (cfg.trace_info_mask.contains(TraceInfoMask::PORT), port) {
        let a = addr.unwrap_or(-1);
        out.text(format_args!("[{port},{a},{reason}] "))?;
    }

    if cfg.trace_info_mask.contains(TraceInfoMask::SOURCE) {
        let (file, line) = source;
        out.text(format_args!("[{}:{}] ", strip_path(file), line))?;
    }

    if cfg.trace_info_mask.contains(TraceInfoMask::THREAD) {
        // C `epicsThreadGetNameSelf()`.
        let name = epics_libcom_rs::runtime::task::thread_name_self();
        out.text(format_args!(
            "[{name},{:#x},{}] ",
            thread_token(),
            thread_epics_priority()
        ))?;
    }

    Ok(())
}

/// C prints `epicsThreadGetIdSelf()` with `%p` — an opaque per-thread token,
/// unique within the process and stable for the thread's life. `ThreadId` is
/// the portable equivalent and carries no public integer, so hash it to one
/// and render it pointer-style. Like a pointer, the value differs between
/// runs; what a reader uses it for — telling two threads apart in one log —
/// is preserved.
fn thread_token() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

/// C prints `epicsThreadGetPrioritySelf()`, the EPICS band (0..=99) stored on
/// the thread at creation — 50 for a `Medium` IOC thread even where the OS
/// refused `SCHED_FIFO`.
///
/// `epics-libcom-rs` bands threads through `runtime::task::enter_ioc_thread`
/// but exposes no read-back, and adding one is a change to that crate rather
/// than to this one. Until it does, this reports 0, which is what a C IOC
/// prints for a thread that took no EPICS band — the tokio worker an asyn
/// port actor runs on today, and not the banded thread it runs on under the
/// exec backend.
fn thread_epics_priority() -> u32 {
    0
}

/// The data section of one `asynPrintIO` line — C `traceVprintIOSource`
/// (asynManager.c:3145-3225), appended to the message line.
///
/// The I/O mask is a **bitfield** and C tests each bit in its own `if`, so the
/// blocks are independent and appear in C's order — ASCII (:3171), ESCAPE
/// (:3178), HEX (:3192). `ASCII|HEX` prints the payload twice; no bit prints no
/// data at all. The port's if/else chain instead picked one form, which meant a
/// two-bit mask silently dropped a block and the empty mask printed ASCII.
///
/// The two gates come straight from C and are not the same gate:
///
/// - ASCII and ESCAPE print only when `nBytes > 0`.
/// - HEX prints when `traceTruncateSize > 0` — including its trailing newline
///   on an empty payload.
/// - A zero mask *or* a zero truncate size emits the bare newline of :3215-3221.
fn print_io_data(p: &mut Printer<'_, '_>, data: &[u8], cfg: &TraceConfig) -> std::fmt::Result {
    // C: `nBytes = (len < traceTruncateSize) ? len : traceTruncateSize` — a
    // truncate size of 0 yields no bytes, it does not mean "unlimited".
    let data = &data[..data.len().min(cfg.io_truncate_size)];
    let mask = cfg.trace_io_mask;

    if mask.contains(TraceIoMask::ASCII) && !data.is_empty() {
        // C `fprintf(fp, "%.*s\n", (int)nBytes, buffer)` — the raw bytes, not a
        // printable-only rendering: a control byte reaches the terminal as
        // itself.
        //
        // The precision caps a `%s` conversion, and `%s` stops at the first
        // NUL: it is an upper bound on the bytes taken from the string, not a
        // count of bytes to copy. So a payload with an embedded NUL prints its
        // head only, and the tail of it never reaches the trace file. ESCAPE
        // and HEX below are byte loops and do print the whole payload — that
        // asymmetry is C's, and is why an operator diagnosing binary traffic
        // is told to turn ESCAPE on.
        let ascii = match data.iter().position(|&b| b == 0) {
            Some(nul) => &data[..nul],
            None => data,
        };
        p.bytes(&[ascii, b"\n"])?;
    }

    if mask.contains(TraceIoMask::ESCAPE) && !data.is_empty() {
        let escaped = format_escape(data, &cfg.file, cfg.trace_buffer_size);
        p.bytes(&[escaped.as_bytes(), b"\n"])?;
    }

    if mask.contains(TraceIoMask::HEX) && cfg.io_truncate_size > 0 {
        // C `"%2.2x "` per byte, a newline before every 20th byte (so the block
        // opens with one) and a newline after the last.
        for (i, b) in data.iter().enumerate() {
            if i % 20 == 0 {
                p.bytes(&[b"\n"])?;
            }
            p.text(format_args!("{b:02x} "))?;
        }
        p.bytes(&[b"\n"])?;
    }

    if mask.is_empty() || cfg.io_truncate_size == 0 {
        p.bytes(&[b"\n"])?;
    }
    Ok(())
}

/// `ASYN_TRACEIO_ESCAPE`. Which of libCom's *two* escape entry points runs is a
/// property of the trace **destination**, not of the mask
/// (`traceVprintIOSource`, asynManager.c:3178-3190):
///
/// ```text
/// fp != NULL   epicsStrPrintEscaped(fp, buffer, nBytes)   stdout/stderr/file
/// fp == NULL   epicsStrSnPrintEscaped(traceBuffer, ...)   errlog
/// ```
///
/// and `getTraceFile` (:2942-2955) returns `NULL` for `traceFileErrlog` alone —
/// every other sink, including the `traceFileStderr` a port is born with
/// (`tracePvtInit`, :460), takes the `FILE *` branch. The two differ in their
/// destination bound (the stream form has none) and in the stream form's
/// first-byte-NUL early-return quirk (R17-49). They no longer differ on the NUL
/// byte: C's stream form printed `\x00` where the errlog form printed `\0`
/// (CBUG-D4), and the port refuses that — both render NUL as `\0`. Hardwiring
/// `escaped_from_raw` gave every sink the errlog form.
///
/// The table itself is one table: its own four-case copy left `\a`, `\b`, `\f`,
/// `\v`, `'` and `"` unescaped or hexed, which no C caller does (R16-48).
fn format_escape(data: &[u8], dest: &TraceFile, buf_size: usize) -> String {
    match dest {
        TraceFile::Errlog => crate::escape::escaped_from_raw(data, buf_size),
        TraceFile::Stderr | TraceFile::Stdout | TraceFile::File(_) => {
            crate::escape::print_escaped(data)
        }
    }
}

/// Log a trace message (checks `is_enabled` first for short-circuit).
///
/// Accepts either `&TraceManager` or `Option<&TraceManager>` as the first argument.
/// When given `Option`, `None` is a silent no-op.
#[macro_export]
macro_rules! asyn_trace {
    (Some($mgr:expr), $port:expr, $mask:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled($port, $mask) {
                __mgr.output_with_source($port, $mask, file!(), line!(), &format!($($arg)*));
            }
        }
    };
    ($mgr:expr, $port:expr, $mask:expr, $($arg:tt)*) => {
        if $mgr.is_enabled($port, $mask) {
            $mgr.output_with_source($port, $mask, file!(), line!(), &format!($($arg)*));
        }
    };
}

/// Log I/O data with formatting.
///
/// Accepts either `&TraceManager` or `Option<&TraceManager>` as the first argument.
/// When given `Option`, `None` is a silent no-op.
#[macro_export]
macro_rules! asyn_trace_io {
    (Some($mgr:expr), $port:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled($port, $mask) {
                __mgr.output_io($port, $mask, $data, &format!($($arg)*), file!(), line!());
            }
        }
    };
    ($mgr:expr, $port:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if $mgr.is_enabled($port, $mask) {
            $mgr.output_io($port, $mask, $data, &format!($($arg)*), file!(), line!());
        }
    };
}

/// Log a per-device trace message (checks `is_enabled_device` first).
///
/// `$addr` is the device address. Both the enable check and the output
/// formatter resolve config in C-parity order: device → port → global
/// (asynManager.c:538-543 / 548-550 / 3092-3098). Use this in drivers that
/// have distinct addresses on a multi-device port; the addr appears in the
/// emitted `[port:addr]` prefix when `TraceInfoMask::PORT` is set.
#[macro_export]
macro_rules! asyn_trace_device {
    (Some($mgr:expr), $port:expr, $addr:expr, $mask:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled_device($port, $addr, $mask) {
                __mgr.output_device_with_source(
                    $port,
                    Some($addr),
                    0,
                    $mask,
                    file!(),
                    line!(),
                    &format!($($arg)*),
                );
            }
        }
    };
    ($mgr:expr, $port:expr, $addr:expr, $mask:expr, $($arg:tt)*) => {
        if $mgr.is_enabled_device($port, $addr, $mask) {
            $mgr.output_device_with_source(
                $port,
                Some($addr),
                0,
                $mask,
                file!(),
                line!(),
                &format!($($arg)*),
            );
        }
    };
}

/// Log per-device I/O data with formatting (C-parity hierarchy).
#[macro_export]
macro_rules! asyn_trace_device_io {
    (Some($mgr:expr), $port:expr, $addr:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled_device($port, $addr, $mask) {
                __mgr.output_device_io(
                    $port,
                    Some($addr),
                    0,
                    $mask,
                    $data,
                    &format!($($arg)*),
                    file!(),
                    line!(),
                );
            }
        }
    };
    ($mgr:expr, $port:expr, $addr:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if $mgr.is_enabled_device($port, $addr, $mask) {
            $mgr.output_device_io(
                $port,
                Some($addr),
                0,
                $mask,
                $data,
                &format!($($arg)*),
                file!(),
                line!(),
            );
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A print past 512 bytes, in text pieces and raw device bytes, reaches
    /// the sink in one `write`, so another thread's line cannot land inside
    /// it. glibc writes an unbuffered `vfprintf` in 128-byte chunks: one
    /// write at any length is a deliberate deviation from C.
    #[test]
    fn a_long_print_is_one_write() {
        struct Writes(Vec<Vec<u8>>);
        impl std::io::Write for Writes {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.push(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Writes(Vec::new());
        let long = "x".repeat(700);
        stream(&mut sink, |out| {
            std::fmt::Write::write_fmt(out, format_args!("{}:{}:", "port", 3))?;
            out.write_bytes(b"\xff\x00")?;
            std::fmt::Write::write_fmt(out, format_args!("{long}\n"))
        });
        let mut want = b"port:3:\xff\x00".to_vec();
        want.extend_from_slice(long.as_bytes());
        want.push(b'\n');
        assert_eq!(sink.0, [want]);
    }

    /// C `vfprintf`s a trace line into the file whole (asynManager.c:3106):
    /// a line far past the stack buffer, written both as one long piece and
    /// as many short ones, lands uncut.
    #[test]
    fn print_writes_a_long_line_uncut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.log");
        let file = std::fs::File::create(&path).unwrap();
        let trace = TraceManager::new();
        trace.set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));
        trace.set_trace_mask(None, TraceMask::ERROR);
        trace.set_trace_info_mask(None, TraceInfoMask::empty());

        let long = "x".repeat(1500);
        let short: Vec<String> = (0..400).map(|i| format!("{i},")).collect();
        trace.print(None, None, 0, TraceMask::ERROR, format_args!("a{long}b\n"));
        trace.print(
            None,
            None,
            0,
            TraceMask::ERROR,
            format_args!("{}\n", Join(&short)),
        );

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, format!("a{long}b\n{}\n", short.concat()));
    }

    /// C's `traceVprintSource` and `traceVprintIOSource` print the caller's
    /// format as given (asynManager.c:3106, :3164): a C format's own `\n`
    /// ends the line, and one without runs on into the next print.
    #[test]
    fn every_print_path_adds_nothing_to_the_callers_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.log");
        let file = std::fs::File::create(&path).unwrap();
        let trace = TraceManager::new();
        trace.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));
        trace.set_trace_mask(Some("p"), TraceMask::ERROR);
        trace.set_trace_info_mask(Some("p"), TraceInfoMask::empty());
        trace.set_trace_io_mask(Some("p"), TraceIoMask::ASCII);

        let e = TraceMask::ERROR;
        trace.print(Some("p"), None, 0, e, format_args!("print\n"));
        trace.output_device("p", None, 0, e, "device\n");
        trace.output_device_with_source("p", None, 0, e, "f.rs", 1, "source\n");
        trace.output_device_io("p", None, 0, e, b"OK", "io\n", "f.rs", 1);
        trace.print(Some("p"), None, 0, e, format_args!("no newline "));
        trace.output_device("p", None, 0, e, "runs on\n");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "print\ndevice\nsource\nio\nOK\nno newline runs on\n");
    }

    /// Displays its pieces with one `write_str` each.
    struct Join<'a>(&'a [String]);

    impl std::fmt::Display for Join<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.iter().try_for_each(|p| f.write_str(p))
        }
    }

    /// C parity: `tracePvtInit` (asynManager.c:456) sets every port's
    /// default mask to `ASYN_TRACE_ERROR` only — WARNING is OFF until a
    /// caller raises it via `asynSetTraceMask`.
    #[test]
    fn test_default_mask_error_only() {
        let mgr = TraceManager::new();
        assert!(mgr.is_enabled("port1", TraceMask::ERROR));
        assert!(!mgr.is_enabled("port1", TraceMask::WARNING));
        assert!(!mgr.is_enabled("port1", TraceMask::FLOW));
        assert!(!mgr.is_enabled("port1", TraceMask::IO_DRIVER));
    }

    /// C asyn defines 6 trace bits in `asynDriver.h:211-216`. This
    /// test fences that we don't accidentally re-introduce extra
    /// bits — `grep -rn "ASYN_TRACE_STATE" $EPICS_MODULES/asyn`
    /// returns 0 hits, so any additional bit would be invented.
    #[test]
    fn test_driver_defined_bits_survive() {
        // asynSetTraceMask takes any int; twincat's ASYN_TRACE_INFO is 0x40.
        let info = TraceMask::from_bits_retain(0x40);
        let m = TraceMask::from_c_mask_string("0x41").0;
        assert_eq!(m.bits(), 0x41);
        assert_eq!(TraceMask::from_bits_truncate(0x41).bits(), 0x41);

        let mgr = TraceManager::new();
        mgr.set_trace_mask(Some("p"), m);
        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_info_bit.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));
        mgr.print(Some("p"), None, 0, info, format_args!("info line"));
        assert!(
            std::fs::read_to_string(&temp)
                .unwrap()
                .contains("info line")
        );
    }

    #[test]
    fn test_io_and_info_masks_keep_every_bit() {
        assert_eq!(TraceIoMask::from_c_mask_string("0x9").0.bits(), 0x9);
        assert_eq!(TraceInfoMask::from_c_mask_string("0x11").0.bits(), 0x11);
        let mgr = TraceManager::new();
        mgr.set_trace_io_mask(Some("p"), TraceIoMask::from_bits_retain(0x9));
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::from_bits_retain(0x11));
        let snap = mgr.snapshot(Some("p"), None);
        assert_eq!(snap.io_mask.bits(), 0x9);
        assert_eq!(snap.info_mask.bits(), 0x11);
    }

    #[test]
    fn test_six_bits_match_c_asyn_header() {
        assert_eq!(TraceMask::ERROR.bits(), 0x0001);
        assert_eq!(TraceMask::IO_DEVICE.bits(), 0x0002);
        assert_eq!(TraceMask::IO_FILTER.bits(), 0x0004);
        assert_eq!(TraceMask::IO_DRIVER.bits(), 0x0008);
        assert_eq!(TraceMask::FLOW.bits(), 0x0010);
        assert_eq!(TraceMask::WARNING.bits(), 0x0020);
        // Union of all 6 = 0x3F.
        let all = TraceMask::ERROR
            | TraceMask::IO_DEVICE
            | TraceMask::IO_FILTER
            | TraceMask::IO_DRIVER
            | TraceMask::FLOW
            | TraceMask::WARNING;
        assert_eq!(all.bits(), 0x003F);
    }

    /// C `asynTraceMaskStringToInt` (asynShellCommands.c:670-699): names with
    /// or without `ASYN_` and `TRACE_`/`TRACEIO_`, any case, `|` or `+`.
    #[test]
    fn trace_mask_string_names_and_prefixes() {
        assert_eq!(
            TraceMask::from_c_mask_string("ERROR|FLOW|DEVICE"),
            (
                TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DEVICE,
                ""
            )
        );
        assert_eq!(
            TraceMask::from_c_mask_string("ASYN_TRACEIO_DRIVER+ASYN_TRACE_FLOW"),
            (TraceMask::IO_DRIVER | TraceMask::FLOW, "")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("error|asyn_traceio_driver|Warning"),
            (
                TraceMask::ERROR | TraceMask::IO_DRIVER | TraceMask::WARNING,
                ""
            )
        );
    }

    /// The tail C reports is where its loop stopped, and the bits before it
    /// stay set: `STARTSWITH` is a prefix match, a stripped `ASYN_` is not
    /// put back, and `strtol(s, 0)` reads `0o20` as `0` then stops.
    #[test]
    fn trace_mask_string_stops_where_c_stops() {
        assert_eq!(
            TraceMask::from_c_mask_string("ERROR|NOPE"),
            (TraceMask::ERROR, "NOPE")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("ERRORS"),
            (TraceMask::ERROR, "S")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("ASYN_FOO"),
            (TraceMask::empty(), "FOO")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("ERROR|0x10|0o20"),
            (TraceMask::ERROR | TraceMask::FLOW, "o20")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("08"),
            (TraceMask::empty(), "8")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("0x"),
            (TraceMask::empty(), "x")
        );
    }

    /// Whitespace is skipped around a name, never after a number, and an empty
    /// field between separators ends the parse.
    #[test]
    fn trace_mask_string_whitespace() {
        assert_eq!(TraceMask::from_c_mask_string(""), (TraceMask::empty(), ""));
        assert_eq!(
            TraceMask::from_c_mask_string("  "),
            (TraceMask::empty(), "")
        );
        assert_eq!(
            TraceMask::from_c_mask_string(" ERROR | | FLOW "),
            (TraceMask::ERROR, "| FLOW ")
        );
        assert_eq!(
            TraceMask::from_c_mask_string("1 | 2"),
            (TraceMask::ERROR, " | 2")
        );
        assert_eq!(
            TraceMask::from_c_mask_string(" 0x21"),
            (TraceMask::ERROR | TraceMask::WARNING, "")
        );
    }

    /// C keeps strtol's long in an int: `-1` and an out-of-range number (glibc
    /// clamps to `LONG_MAX`) both set every bit.
    #[test]
    fn trace_mask_string_number_is_cs_int() {
        assert_eq!(TraceMask::from_c_mask_string("-1").0.bits(), u32::MAX);
        assert_eq!(
            TraceMask::from_c_mask_string("0xfffffffffffffffffff")
                .0
                .bits(),
            u32::MAX
        );
        assert_eq!(TraceMask::from_c_mask_string("-0x10").0.bits(), 0xffff_fff0);
    }

    #[test]
    fn trace_io_and_info_mask_strings() {
        assert_eq!(
            TraceIoMask::from_c_mask_string("ESCAPE+HEX"),
            (TraceIoMask::ESCAPE | TraceIoMask::HEX, "")
        );
        assert_eq!(
            TraceIoMask::from_c_mask_string("ASYN_TRACEIO_ASCII"),
            (TraceIoMask::ASCII, "")
        );
        // ASYN_TRACEIO_NODATA is 0 (asynDriver.h:219).
        assert_eq!(
            TraceIoMask::from_c_mask_string("NODATA+HEX"),
            (TraceIoMask::HEX, "")
        );
        // TRACE_ is not an I/O prefix.
        assert_eq!(
            TraceIoMask::from_c_mask_string("TRACE_HEX"),
            (TraceIoMask::empty(), "TRACE_HEX")
        );
        assert_eq!(
            TraceInfoMask::from_c_mask_string("TIME|asyn_traceinfo_thread"),
            (TraceInfoMask::TIME | TraceInfoMask::THREAD, "")
        );
    }

    /// A port is born with only the TIME trace-info bit (C `tracePvtInit`,
    /// asynManager.c:457 `traceInfoMask = ASYN_TRACEINFO_TIME`), so an asyn
    /// record over a fresh port reads back `TINM=1` / `TINB1(port)=Off` — not
    /// `TIME | PORT` (=3, which lit `TINB1=On`).
    #[test]
    fn fresh_port_trace_info_mask_is_time_only() {
        assert_eq!(TraceConfig::default().trace_info_mask, TraceInfoMask::TIME);
        let mgr = TraceManager::new();
        // Unknown port falls back to the global default — also TIME only.
        assert_eq!(
            mgr.get_trace_info_mask(Some("never-created")),
            TraceInfoMask::TIME
        );
        assert_eq!(mgr.get_trace_info_mask(None), TraceInfoMask::TIME);
        // The PORT bit is therefore off until an operator sets it.
        assert!(
            !TraceConfig::default()
                .trace_info_mask
                .contains(TraceInfoMask::PORT)
        );
    }

    /// `asynSetTraceMask` with no port name passes `pasynUser = NULL`
    /// (asynShellCommands.c:646-660), so `setTraceMask` writes
    /// `pasynBase->trace` and nothing else (asynManager.c:2789-2791). The
    /// global slot is read back by `getTraceMask(NULL)` and by no port.
    #[test]
    fn test_set_global_mask() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::ERROR | TraceMask::FLOW);
        assert_eq!(mgr.get_trace_mask(None), TraceMask::ERROR | TraceMask::FLOW);
        // ...and a port keeps its `tracePvtInit` birth mask (:456).
        assert!(mgr.is_enabled("any", TraceMask::ERROR));
        assert!(!mgr.is_enabled("any", TraceMask::FLOW));
    }

    #[test]
    fn test_port_override_vs_global() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::ERROR);
        mgr.set_trace_mask(Some("myport"), TraceMask::FLOW);

        // myport uses its override
        assert!(mgr.is_enabled("myport", TraceMask::FLOW));
        assert!(!mgr.is_enabled("myport", TraceMask::ERROR));

        // other ports use global
        assert!(mgr.is_enabled("other", TraceMask::ERROR));
        assert!(!mgr.is_enabled("other", TraceMask::FLOW));
    }

    /// [`print_io_data`] on a stream sink.
    fn append_io_data(out: &mut Vec<u8>, data: &[u8], cfg: &TraceConfig) {
        stream(out, |o| print_io_data(&mut Printer::Stream(o), data, cfg));
    }

    /// The data section C's `traceVprintIOSource` appends to one `asynPrintIO`
    /// message, for a config that differs from the default only in the I/O mask
    /// and the truncate size.
    fn blocks(data: &[u8], mask: TraceIoMask, io_truncate_size: usize) -> Vec<u8> {
        let cfg = TraceConfig {
            trace_io_mask: mask,
            io_truncate_size,
            ..TraceConfig::default()
        };
        let mut out = Vec::new();
        append_io_data(&mut out, data, &cfg);
        out
    }

    /// R17-47. `traceIOMask` is a bitfield: C runs one independent `if` block
    /// per set bit, in the order ASCII (asynManager.c:3171), ESCAPE (:3178),
    /// HEX (:3192). Two bits print the payload twice; the port's if/else chain
    /// printed one form and silently dropped the other.
    #[test]
    fn every_enabled_io_mask_bit_emits_its_own_block_in_c_s_order() {
        let data = b"OK\r\n";

        assert_eq!(blocks(data, TraceIoMask::ASCII, 80), b"OK\r\n\n");
        assert_eq!(blocks(data, TraceIoMask::ESCAPE, 80), b"OK\\r\\n\n");
        assert_eq!(blocks(data, TraceIoMask::HEX, 80), b"\n4f 4b 0d 0a \n");

        // Two bits — both blocks, ASCII first.
        assert_eq!(
            blocks(data, TraceIoMask::ASCII | TraceIoMask::HEX, 80),
            b"OK\r\n\n\n4f 4b 0d 0a \n"
        );
        // All three.
        assert_eq!(
            blocks(data, TraceIoMask::all(), 80),
            b"OK\r\n\nOK\\r\\n\n\n4f 4b 0d 0a \n"
        );
    }

    /// C's ASCII block is `fprintf(fp, "%.*s\n", (int)nBytes, buffer)`
    /// (asynManager.c:3173) — the device bytes, verbatim. The port substituted
    /// `.` for every non-printable byte, which is a rendering C never does (and
    /// is what ASYN_TRACEIO_ESCAPE exists for).
    #[test]
    fn the_ascii_block_is_the_raw_bytes_not_a_printable_rendering() {
        assert_eq!(blocks(b"hi\r\n", TraceIoMask::ASCII, 80), b"hi\r\n\n");
        // …but only up to the first NUL: `%.*s` bounds a `%s` conversion and
        // `%s` stops there, so a leading-NUL payload prints an empty data
        // line. This case used to assert the whole slice, which was the
        // R18-67 defect written down as an expectation.
        assert_eq!(blocks(&[0x00, 0x7f, 0x41], TraceIoMask::ASCII, 80), b"\n");
        // Invalid UTF-8 reaches the sink as itself.
        assert_eq!(
            blocks(&[0xff, 0xfe], TraceIoMask::ASCII, 80),
            &[0xff, 0xfe, b'\n']
        );
    }

    /// C's HEX block: a newline before every 20th byte — so the block opens with
    /// one — `"%2.2x "` per byte, and a closing newline (asynManager.c:3192-3211).
    /// The port emitted one unwrapped space-joined run with no newlines at all.
    #[test]
    fn the_hex_block_wraps_every_twenty_bytes_and_is_newline_wrapped() {
        let data: Vec<u8> = (0..25).collect();
        let out = String::from_utf8(blocks(&data, TraceIoMask::HEX, 80)).unwrap();

        let mut want = String::from("\n");
        for b in 0..20u8 {
            want.push_str(&format!("{b:02x} "));
        }
        want.push('\n');
        for b in 20..25u8 {
            want.push_str(&format!("{b:02x} "));
        }
        want.push('\n');
        assert_eq!(out, want);

        // An empty payload still prints the trailing newline: C gates the HEX
        // block on traceTruncateSize, not on nBytes (:3192).
        assert_eq!(blocks(b"", TraceIoMask::HEX, 80), b"\n");
    }

    /// C's two no-data paths (asynManager.c:3215-3221): a zero mask, or a zero
    /// truncate size, emits a bare newline and nothing else. The port defaulted
    /// to ASCII on an empty mask and read a zero truncate size as "unlimited".
    #[test]
    fn a_zero_mask_or_a_zero_truncate_size_emits_a_bare_newline() {
        assert_eq!(blocks(b"OK", TraceIoMask::NODATA, 80), b"\n");
        assert_eq!(blocks(b"OK", TraceIoMask::ASCII, 0), b"\n");
        assert_eq!(blocks(b"OK", TraceIoMask::all(), 0), b"\n");

        // And NODATA is what a port starts with — `tracePvtInit` leaves
        // traceIOMask at the calloc zero (asynManager.c:451-461).
        assert_eq!(TraceConfig::default().trace_io_mask, TraceIoMask::NODATA);
        assert_eq!(
            TraceManager::new().get_trace_io_mask(None),
            TraceIoMask::NODATA
        );
    }

    /// C truncates the *payload* at traceTruncateSize before any block runs
    /// (`nBytes = min(len, traceTruncateSize)`), so every enabled form shows the
    /// same prefix of the data.
    #[test]
    fn the_truncate_size_bounds_every_block() {
        assert_eq!(blocks(b"hello world", TraceIoMask::ASCII, 4), b"hell\n");
        assert_eq!(blocks(b"hello world", TraceIoMask::ESCAPE, 4), b"hell\n");
        assert_eq!(
            blocks(b"hello world", TraceIoMask::HEX, 4),
            b"\n68 65 6c 6c \n"
        );
    }

    /// R17-49 on the trace line. The ESCAPE block runs (C gates it on
    /// `nBytes > 0`, asynManager.c:3153) but `epicsStrPrintEscaped` writes
    /// nothing for a payload whose first byte is NUL (epicsString.c:237-238),
    /// so a `FILE *` sink gets an *empty* data line — the block's newline and
    /// no bytes. The errlog sink escapes it as `\0…` instead.
    #[test]
    fn a_first_byte_nul_payload_escapes_to_an_empty_data_line_on_a_file_sink() {
        assert_eq!(blocks(b"\0ab", TraceIoMask::ESCAPE, 80), b"\n");

        let cfg = TraceConfig {
            trace_io_mask: TraceIoMask::ESCAPE,
            file: TraceFile::Errlog,
            ..TraceConfig::default()
        };
        let mut out = Vec::new();
        append_io_data(&mut out, b"\0ab", &cfg);
        assert_eq!(out, b"\\0ab\n");
    }

    #[test]
    fn test_format_escape() {
        let n = DEFAULT_TRACE_BUFFER_SIZE;
        let errlog = TraceFile::Errlog;
        assert_eq!(format_escape(b"OK\r\n", &errlog, n), "OK\\r\\n");
        assert_eq!(format_escape(b"\t\\", &errlog, n), "\\t\\\\");
        assert_eq!(format_escape(&[0x01], &errlog, n), "\\x01");
        assert_eq!(format_escape(b"hi", &errlog, n), "hi");
    }

    /// The ESCAPE form's destination is C's `tracePvt.traceBuffer`
    /// (asynManager.c:3184), 80 bytes until `setTraceIOTruncateSize` grows it
    /// (:2963-2979) — so an escape-heavy errlog line is cut at
    /// `traceBufferSize - 1`. The stream branch has no such buffer.
    #[test]
    fn format_escape_is_bounded_by_the_trace_buffer_on_the_errlog_branch_only() {
        let crlf: Vec<u8> = b"\r\n".repeat(50);
        let out = format_escape(&crlf, &TraceFile::Errlog, DEFAULT_TRACE_BUFFER_SIZE);
        assert_eq!(out.len(), DEFAULT_TRACE_BUFFER_SIZE - 1);
        assert!(out.ends_with(r"\r\n\r\"), "cut mid-pair, as C does: {out}");

        let out = format_escape(&crlf, &TraceFile::Stderr, DEFAULT_TRACE_BUFFER_SIZE);
        assert_eq!(out.len(), 200, "epicsStrPrintEscaped writes to a stream");
    }

    /// R17-46. C picks the escape entry point by *destination*
    /// (asynManager.c:3178-3190): `fp != NULL` → `epicsStrPrintEscaped`,
    /// errlog → `epicsStrSnPrintEscaped`. `getTraceFile` (:2942-2955) hands back
    /// `NULL` for errlog alone, and a port's default sink is stderr
    /// (`tracePvtInit`, :460) — so the *default* trace line takes the stream
    /// form. C's stream form printed a NUL as `\x00` where the errlog form
    /// printed `\0` (CBUG-D4); the port refuses that divergence, so every sink
    /// now renders NUL as `\0`. The destination still selects the entry point
    /// (bound / first-byte-NUL quirk differ); the escape table does not.
    #[test]
    fn the_escape_entry_point_is_chosen_by_the_trace_destination() {
        let n = DEFAULT_TRACE_BUFFER_SIZE;
        let data = b"a\0b";

        // errlog: epicsStrSnPrintEscaped — has `case '\0'` (epicsString.c:145).
        assert_eq!(format_escape(data, &TraceFile::Errlog, n), r"a\0b");

        // Every FILE* sink, the stderr default included: epicsStrPrintEscaped.
        // C left it without a NUL case (:255-260) and printed `\x00`; the port
        // supplies the missing case (CBUG-D4 refused) so it matches errlog: `\0`.
        assert_eq!(format_escape(data, &TraceFile::Stderr, n), r"a\0b");
        assert_eq!(format_escape(data, &TraceFile::Stdout, n), r"a\0b");
        let dir = tempfile::tempdir().expect("fixture root");
        let f = TraceFile::File(Arc::new(Mutex::new(
            std::fs::File::create(dir.path().join("asyn_r17_46.txt")).unwrap(),
        )));
        assert_eq!(format_escape(data, &f, n), r"a\0b");
        let _ = std::fs::remove_file(dir.path().join("asyn_r17_46.txt"));

        // And the default TraceConfig is one of the FILE* sinks, not errlog.
        assert!(matches!(TraceConfig::default().file, TraceFile::Stderr));
    }

    /// C `setTraceIOTruncateSize` reallocates `traceBuffer` to the new size when
    /// it exceeds the current one, and never shrinks it back
    /// (asynManager.c:2963-2979) — so a bigger truncate size widens the ESCAPE
    /// bound, and a smaller one afterwards does not narrow it.
    #[test]
    fn a_bigger_truncate_size_grows_the_trace_buffer_and_a_smaller_one_does_not_shrink_it() {
        let mgr = TraceManager::new();
        mgr.set_io_truncate_size(None, 400).unwrap();
        assert_eq!(mgr.global_config.lock().unwrap().trace_buffer_size, 400);
        mgr.set_io_truncate_size(None, 8).unwrap();
        assert_eq!(mgr.global_config.lock().unwrap().io_truncate_size, 8);
        assert_eq!(mgr.global_config.lock().unwrap().trace_buffer_size, 400);
    }

    /// A negative `epicsInt32` TSIZ reaches C `setTraceIOTruncateSize` as a
    /// `size_t` no `calloc` can satisfy, and C returns `asynError` before it
    /// writes or announces anything (asynManager.c:2963-2975). Every rung.
    #[test]
    fn an_unallocatable_truncate_size_is_refused_with_nothing_changed() {
        use crate::exception::AsynException;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering as O;

        let exc = Arc::new(ExceptionManager::new());
        let mgr = TraceManager::new();
        mgr.set_exception_sink(exc.clone());
        mgr.register_port("p", true);
        mgr.set_io_truncate_size(Some("p"), 100).unwrap();
        mgr.set_device_io_truncate_size("p", 0, 100).unwrap();
        mgr.set_io_truncate_size(None, 100).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        exc.add_callback(move |ev| {
            if ev.exception == AsynException::TraceIoTruncateSize {
                h2.fetch_add(1, O::Relaxed);
            }
        });

        for tsiz in [-1i32, i32::MIN] {
            let refused = Err(TruncateSizeRefused(tsiz as usize));
            assert_eq!(mgr.set_io_truncate_size(Some("p"), tsiz as usize), refused);
            assert_eq!(
                mgr.set_device_io_truncate_size("p", 0, tsiz as usize),
                refused
            );
            assert_eq!(mgr.set_io_truncate_size(None, tsiz as usize), refused);
        }

        assert_eq!(mgr.snapshot(Some("p"), None).io_truncate_size, 100);
        assert_eq!(mgr.snapshot(Some("p"), Some(0)).io_truncate_size, 100);
        let global = mgr.global_config.lock().unwrap();
        assert_eq!(global.io_truncate_size, 100);
        assert_eq!(global.trace_buffer_size, 100);
        assert_eq!(hits.load(O::Relaxed), 0, "a refusal announces nothing");
    }

    #[test]
    fn test_output_to_buffer() {
        let mgr = TraceManager::new();
        // `asynSetTraceMask testport -1 ...` — the port's own dpCommon, which
        // is the only slot the emit path reads (asynManager.c:538-553).
        mgr.set_trace_mask(Some("testport"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(Some("testport"), TraceInfoMask::PORT); // only port name for predictability

        // Create a shared buffer as a file
        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(
            Some("testport"),
            TraceFile::File(Arc::new(Mutex::new(file))),
        );

        mgr.output("testport", TraceMask::ERROR, "something broke");

        // Read back
        let contents = std::fs::read_to_string(&temp).unwrap();
        // C `printPort` writes the whole `[port,addr,reason]` triple
        // (asynManager.c:3043), and `getAddr` (:2006) yields -1 for the
        // port-level user `output` stands in for. No severity token: C
        // prints none.
        assert!(contents.contains("[testport,-1,0] "), "got {contents:?}");
        assert!(!contents.contains("ERROR"), "C prints no mask label");
        assert!(contents.contains("something broke"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_output_io_to_buffer() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(Some("testport"), TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(Some("testport"), TraceInfoMask::PORT);
        mgr.set_trace_io_mask(Some("testport"), TraceIoMask::ESCAPE);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_io_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(
            Some("testport"),
            TraceFile::File(Arc::new(Mutex::new(file))),
        );

        mgr.output_io("testport", TraceMask::IO_DRIVER, b"OK\r\n", "read:", "", 0);

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert!(contents.contains("[testport,-1,0] "), "got {contents:?}");
        assert!(!contents.contains("IO_DRIVER"), "C prints no mask label");
        assert!(contents.contains("read:"));
        assert!(contents.contains("OK\\r\\n"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_get_masks() {
        let mgr = TraceManager::new();
        // Default global mask is ERROR-only (asynManager.c:456), and the I/O
        // mask is the calloc zero `tracePvtInit` leaves behind (:451-461).
        assert_eq!(mgr.get_trace_mask(None), TraceMask::ERROR);
        assert_eq!(mgr.get_trace_io_mask(None), TraceIoMask::NODATA);

        mgr.set_trace_mask(Some("p1"), TraceMask::FLOW);
        assert_eq!(mgr.get_trace_mask(Some("p1")), TraceMask::FLOW);
        // Global unaffected
        assert_eq!(mgr.get_trace_mask(None), TraceMask::ERROR);
    }

    #[test]
    fn test_macro_short_circuit() {
        let mgr = TraceManager::new();
        // FLOW is not enabled by default
        // This should not panic or produce output
        asyn_trace!(mgr, "port", TraceMask::FLOW, "should not appear");
    }

    #[test]
    fn test_io_truncate_integration() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(Some("p"), TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::PORT);
        // An I/O form is an operator's choice — a port has none by default.
        mgr.set_trace_io_mask(Some("p"), TraceIoMask::ASCII);
        mgr.set_io_truncate_size(Some("p"), 3).unwrap();

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_trunc_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_io("p", TraceMask::IO_DRIVER, b"hello world", "write:", "", 0);

        let contents = std::fs::read_to_string(&temp).unwrap();
        // ASCII format, truncated to 3 bytes: "hel"
        assert!(contents.contains("hel"));
        assert!(!contents.contains("hello"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_write_line_single_call() {
        // Verify that File variant does a single write_all
        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_single_write.txt");
        let file = std::fs::File::create(&temp).unwrap();
        let tf = TraceFile::File(Arc::new(Mutex::new(file)));

        tf.write_line("line one\n");
        tf.write_line("line two\n");

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert_eq!(contents, "line one\nline two\n");
        let _ = std::fs::remove_file(&temp);
    }

    /// C parity regression: every `setTrace*` mutator must fire its
    /// matching `asynExceptionTrace*` to the exception sink, matching
    /// C asynManager.c:2804/2846/2888/2937/2981. Without this,
    /// listeners (asynShellCommands UI, asynRecord, monitor sinks)
    /// never see the trace-config change.
    #[test]
    fn test_set_trace_mask_fires_exception() {
        use crate::exception::AsynException;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering as O;

        let exc = Arc::new(ExceptionManager::new());
        let mgr = TraceManager::new();
        mgr.set_exception_sink(exc.clone());

        let n = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::<AsynException>::new()));
        let n2 = n.clone();
        let captured2 = captured.clone();
        exc.add_callback(move |ev| {
            n2.fetch_add(1, O::Relaxed);
            captured2.lock().unwrap().push(ev.exception);
        });

        // Each setter fires exactly its own exception type.
        mgr.set_trace_mask(Some("p"), TraceMask::FLOW);
        mgr.set_trace_io_mask(Some("p"), TraceIoMask::HEX);
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::TIME);
        let file = TraceFile::Stderr;
        mgr.set_trace_file(Some("p"), file);
        mgr.set_io_truncate_size(Some("p"), 16).unwrap();
        mgr.set_device_trace_mask("p", 3, TraceMask::ERROR);

        assert_eq!(n.load(O::Relaxed), 6);
        let exps = captured.lock().unwrap().clone();
        assert!(exps.contains(&AsynException::TraceMask));
        assert!(exps.contains(&AsynException::TraceIoMask));
        assert!(exps.contains(&AsynException::TraceInfoMask));
        assert!(exps.contains(&AsynException::TraceFile));
        assert!(exps.contains(&AsynException::TraceIoTruncateSize));
    }

    /// C parity: `setTraceMask` with no pasynUser is the "global"
    /// path (asynManager.c:2788-2790). Rust mirrors with
    /// `set_trace_mask(None, ...)`. The global path still announces
    /// (asynManager.c:2814 announces `pport=NULL` per-port and
    /// 2790/2796 announces per-device; for the "no user" entrypoint
    /// C skips into the global slot at line 2776 without firing —
    /// matching that, our `None` path fires once with an empty port
    /// name so listeners can still observe a global re-config).
    #[test]
    fn test_global_trace_mask_announce() {
        use crate::exception::AsynException;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering as O;

        let exc = Arc::new(ExceptionManager::new());
        let mgr = TraceManager::new();
        mgr.set_exception_sink(exc.clone());
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        exc.add_callback(move |ev| {
            if ev.exception == AsynException::TraceMask && ev.port_name.is_empty() {
                n2.fetch_add(1, O::Relaxed);
            }
        });
        mgr.set_trace_mask(None, TraceMask::FLOW);
        assert_eq!(n.load(O::Relaxed), 1);
    }

    /// `setTraceFile` and `setTraceIOTruncateSize` only announce when
    /// `puserPvt->pport` is non-null (asynManager.c:2937, :2981).
    /// Our `None` (= "no user, global") path therefore must NOT fire
    /// those two exceptions.
    #[test]
    fn test_global_file_and_truncate_do_not_announce() {
        use crate::exception::AsynException;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering as O;

        let exc = Arc::new(ExceptionManager::new());
        let mgr = TraceManager::new();
        mgr.set_exception_sink(exc.clone());
        let file_hits = Arc::new(AtomicUsize::new(0));
        let trunc_hits = Arc::new(AtomicUsize::new(0));
        let f2 = file_hits.clone();
        let t2 = trunc_hits.clone();
        exc.add_callback(move |ev| match ev.exception {
            AsynException::TraceFile => {
                f2.fetch_add(1, O::Relaxed);
            }
            AsynException::TraceIoTruncateSize => {
                t2.fetch_add(1, O::Relaxed);
            }
            _ => {}
        });
        mgr.set_trace_file(None, TraceFile::Stderr);
        mgr.set_io_truncate_size(None, 32).unwrap();
        assert_eq!(file_hits.load(O::Relaxed), 0);
        assert_eq!(trunc_hits.load(O::Relaxed), 0);
    }

    // ----------------------------------------------------------------
    // C-parity: output_device / output_device_with_source / output_device_io
    // must resolve config device → port → global. Previously the
    // port-only output_*() walked port→global, ignoring per-device
    // overrides — `is_enabled_device` saw the device level but the
    // emit path did not. asynManager.c:538-543, 548-550, 3092-3098, 3148-3158.
    // ----------------------------------------------------------------

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn output_device_uses_device_config_when_present() {
        let mgr = TraceManager::new();
        // A device slot is addressable only on an ASYN_MULTIDEVICE port
        // (C `locateDevice`, asynManager.c:576), so the port has to say so
        // before any of the per-device writes below name a device.
        mgr.register_port("dev_p", true);
        // Port allows ERROR; device allows ERROR | FLOW.
        mgr.set_trace_mask(Some("dev_p"), TraceMask::ERROR);
        mgr.set_device_trace_mask("dev_p", 5, TraceMask::ERROR | TraceMask::FLOW);
        // The `[port:addr]` prefix is driven by the *effective* config's info
        // mask, which for a device is the device's own slot (whole-config
        // resolution, not per-field merge). A fresh port/device carries only
        // TIME, so turn PORT on where the emit actually reads it.
        mgr.set_device_trace_info_mask("dev_p", 5, TraceInfoMask::PORT);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_device_output.txt");
        let file = std::fs::File::create(&temp).unwrap();
        let tf = TraceFile::File(Arc::new(Mutex::new(file)));
        // Install the device-specific file so we can verify the device
        // config is what's used (not the port config).
        let file2 = std::fs::File::create(&temp).unwrap();
        let tf2 = TraceFile::File(Arc::new(Mutex::new(file2)));
        mgr.set_device_trace_file("dev_p", 5, tf);
        mgr.set_trace_file(Some("dev_p"), tf2);

        // FLOW is enabled at device, disabled at port — output_device
        // must use the device config and emit.
        mgr.output_device("dev_p", Some(5), 0, TraceMask::FLOW, "device-flow");

        let lines = read_lines(&temp);
        assert!(
            lines.iter().any(|l| l.contains("device-flow")),
            "device-config output should have been emitted, got {lines:?}"
        );
        // Prefix should embed addr.
        assert!(
            lines.iter().any(|l| l.contains("[dev_p,5,0] ")),
            "device output prefix should embed addr, got {lines:?}"
        );
        let _ = std::fs::remove_file(&temp);
    }

    /// A device address with no device configuration reads the port's own
    /// `dpCommon`, and that is where the port-scoped writes below landed:
    /// `findDpCommon` picks one struct with no chain behind it
    /// (asynManager.c:538-545), and a port-scoped write pushed itself into
    /// every device slot, so there is nothing for a chain to reach past.
    /// The prefix still carries the addr the caller named.
    #[test]
    fn output_device_with_no_device_config_reads_the_port_slot() {
        let mgr = TraceManager::new();
        mgr.register_port("no_overrides", true);
        mgr.set_trace_info_mask(Some("no_overrides"), TraceInfoMask::PORT);
        mgr.set_trace_mask(Some("no_overrides"), TraceMask::ERROR);
        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_device_fallback.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(
            Some("no_overrides"),
            TraceFile::File(Arc::new(Mutex::new(file))),
        );

        mgr.output_device("no_overrides", Some(0), 0, TraceMask::ERROR, "port-error");
        let lines = read_lines(&temp);
        assert!(lines.iter().any(|l| l.contains("port-error")));
        assert!(lines.iter().any(|l| l.contains("[no_overrides,0,0] ")));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn output_device_with_source_includes_source_when_device_info_mask_has_it() {
        let mgr = TraceManager::new();
        mgr.register_port("p", true);
        mgr.set_trace_mask(Some("p"), TraceMask::ERROR);
        // Device config carries the SOURCE info bit; port does not.
        mgr.set_device_trace_mask("p", 1, TraceMask::ERROR);
        mgr.set_device_trace_info_mask("p", 1, TraceInfoMask::PORT | TraceInfoMask::SOURCE);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_device_source.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_device_trace_file("p", 1, TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_device_with_source("p", Some(1), 0, TraceMask::ERROR, "src.rs", 42, "msg");
        let lines = read_lines(&temp);
        assert!(
            lines.iter().any(|l| l.contains("[src.rs:42]")),
            "device cfg with SOURCE bit should emit `[file:line]` prefix"
        );
        let _ = std::fs::remove_file(&temp);
    }

    /// C's `asynPrint` is a macro that hands `__FILE__`/`__LINE__` to
    /// `printSource` (asynDriver.h:296-306), so a SOURCE prefix names the
    /// call site, not `[:0]`.
    #[test]
    fn a_plain_print_names_its_caller() {
        let mgr = TraceManager::new();
        mgr.register_port("p", false);
        mgr.set_trace_mask(Some("p"), TraceMask::ERROR);
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::SOURCE);
        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_caller.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));

        let at = line!() + 1;
        mgr.print(Some("p"), None, 0, TraceMask::ERROR, format_args!("a\n"));
        mgr.output("p", TraceMask::ERROR, "b\n");
        mgr.output_device("p", None, 0, TraceMask::ERROR, "c\n");
        let log = std::fs::read_to_string(&temp).unwrap();
        assert_eq!(
            log,
            format!(
                "[trace.rs:{at}] a\n[trace.rs:{}] b\n[trace.rs:{}] c\n",
                at + 1,
                at + 2
            )
        );
    }

    #[test]
    fn output_device_io_uses_device_truncate() {
        // C parity: per-device traceTruncateSize takes priority — the
        // emit path resolves config device → port → global.
        let mgr = TraceManager::new();
        mgr.register_port("p", true);
        mgr.set_trace_mask(Some("p"), TraceMask::IO_DRIVER);
        mgr.set_io_truncate_size(Some("p"), 64).unwrap();
        // Device override: truncate to 3 bytes.
        mgr.set_device_trace_mask("p", 0, TraceMask::IO_DRIVER);
        mgr.set_device_io_truncate_size("p", 0, 3).unwrap();
        mgr.set_device_trace_info_mask("p", 0, TraceInfoMask::PORT);
        mgr.set_device_trace_io_mask("p", 0, TraceIoMask::ASCII);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_device_trunc.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_device_trace_file("p", 0, TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_device_io(
            "p",
            Some(0),
            0,
            TraceMask::IO_DRIVER,
            b"hello world",
            "rx:",
            "",
            0,
        );
        let contents = std::fs::read_to_string(&temp).unwrap_or_default();
        assert!(contents.contains("hel"));
        // 4th byte onward must be dropped by device-level truncation.
        assert!(!contents.contains("hello"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn asyn_trace_device_macro_short_circuits_when_device_disabled() {
        // Macro gates on is_enabled_device; if no level is on, the
        // format!() side-effect must not even run (cheap test: ensure
        // it doesn't panic on a closed configuration).
        let mgr = TraceManager::new();
        mgr.register_port("p", true);
        // Globally only ERROR; device explicitly disables ERROR.
        mgr.set_device_trace_mask("p", 0, TraceMask::empty());
        asyn_trace_device!(mgr, "p", 0, TraceMask::ERROR, "should-not-emit");
        asyn_trace_device_io!(mgr, "p", 0, TraceMask::ERROR, b"data", "rx:");
    }

    #[test]
    fn asyn_trace_device_macro_emits_when_device_enables_flow() {
        let mgr = TraceManager::new();
        mgr.register_port("p", true);
        // Port disables FLOW; device enables FLOW.
        mgr.set_trace_mask(Some("p"), TraceMask::ERROR);
        mgr.set_device_trace_mask("p", 7, TraceMask::FLOW);
        // Prefix reads the device's own info mask (whole-config resolution);
        // a fresh device carries only TIME, so enable PORT on the device slot.
        mgr.set_device_trace_info_mask("p", 7, TraceInfoMask::PORT);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_device_macro.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_device_trace_file("p", 7, TraceFile::File(Arc::new(Mutex::new(file))));

        asyn_trace_device!(mgr, "p", 7, TraceMask::FLOW, "{}", "device-msg");
        let contents = std::fs::read_to_string(&temp).unwrap_or_default();
        assert!(contents.contains("device-msg"));
        assert!(contents.contains("[p,7,0] "));
        let _ = std::fs::remove_file(&temp);
    }

    /// R18-67. C's ASCII block is `fprintf(fp, "%.*s\n", nBytes, buffer)`
    /// (asynManager.c:3173). The precision bounds a `%s` conversion, and
    /// `%s` stops at the first NUL, so the tail after an embedded NUL is
    /// never printed. ESCAPE and HEX are byte loops (:3178-3190, :3192-3212)
    /// and do print it — the asymmetry is C's.
    #[test]
    fn the_ascii_block_stops_at_an_embedded_nul_where_escape_does_not() {
        let payload = b"head\0tail";

        let cfg = TraceConfig {
            trace_io_mask: TraceIoMask::ASCII,
            io_truncate_size: 80,
            ..TraceConfig::default()
        };
        let mut out = Vec::new();
        append_io_data(&mut out, payload, &cfg);
        assert_eq!(
            out, b"head\n",
            "the ASCII block stops at the NUL, got {out:?}"
        );

        // The same payload under ESCAPE renders every byte, NUL included.
        let cfg = TraceConfig {
            trace_io_mask: TraceIoMask::ESCAPE,
            io_truncate_size: 80,
            ..TraceConfig::default()
        };
        let mut out = Vec::new();
        append_io_data(&mut out, payload, &cfg);
        assert_eq!(out, b"head\\0tail\n", "ESCAPE is a byte loop, got {out:?}");
    }

    /// R18-66. `ASYN_TRACEINFO_SOURCE` on the `asynPrintIO` path. C's
    /// `asynPrintIO` captures `__FILE__`/`__LINE__` (asynDriver.h:296-299)
    /// and `traceVprintIOSource` prints them through `printSource`
    /// (asynManager.c:3163), so an I/O trace carries a source component
    /// exactly as a message trace does.
    #[test]
    fn a_source_only_print_io_emits_the_file_and_line() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(Some("p"), TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::SOURCE);
        mgr.set_trace_io_mask(Some("p"), TraceIoMask::ASCII);

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_io_source.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_io(
            "p",
            TraceMask::IO_DRIVER,
            b"OK",
            "read 2 bytes\n",
            "crates/asyn-rs/src/drivers/serial_port.rs",
            1355,
        );

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert!(
            contents.starts_with("[serial_port.rs:1355] read 2 bytes\n"),
            "SOURCE is the only info bit, so it is the whole prefix: {contents:?}"
        );
        let _ = std::fs::remove_file(&temp);
    }

    /// R18-64. The prefix is C's, component by component: `printTime`
    /// (asynManager.c:3009-3028), `printPort` (:3030-3048), `printSource`
    /// (:3050-3061) and `printThread` (:2994-3007), tested in the order
    /// `traceVprintSource` tests their bits (:3101-3104).
    #[test]
    fn the_prefix_is_c_s_four_components_in_c_s_order() {
        let mgr = TraceManager::new();
        // getAddr reports the address only on a multi-device port.
        mgr.register_port("p", true);
        mgr.set_trace_mask(Some("p"), TraceMask::ERROR);
        mgr.set_trace_info_mask(
            Some("p"),
            TraceInfoMask::TIME
                | TraceInfoMask::PORT
                | TraceInfoMask::SOURCE
                | TraceInfoMask::THREAD,
        );

        let dir = tempfile::tempdir().expect("fixture root");
        let temp = dir.path().join("asyn_trace_prefix.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_device_with_source(
            "p",
            Some(3),
            7,
            TraceMask::ERROR,
            "crates/asyn-rs/src/drivers/ip_port.rs",
            871,
            "read 2 bytes",
        );

        let line = read_lines(&temp).remove(0);

        // TIME: `%Y/%m/%d %H:%M:%S.%03f`, not the raw epoch this used to emit.
        // `YYYY/MM/DD HH:MM:SS.mmm ` — 23 characters plus the trailing space
        // C's `fprintf(fp, "%s ", nowText)` writes.
        let (time, rest) = line.split_at(24);
        assert!(
            time.as_bytes()[4] == b'/'
                && time.as_bytes()[7] == b'/'
                && time.as_bytes()[10] == b' '
                && time.as_bytes()[13] == b':'
                && time.as_bytes()[16] == b':'
                && time.as_bytes()[19] == b'.'
                && time.as_bytes()[23] == b' ',
            "strftime TIME, got {time:?}"
        );
        assert!(
            time[..4].chars().all(|c| c.is_ascii_digit()),
            "a four-digit year, not an epoch second count: {time:?}"
        );

        // PORT carries addr AND reason; SOURCE is stripped to a basename and
        // sits between PORT and THREAD; THREAD carries id and priority.
        let rest = rest
            .strip_prefix("[p,3,7] ")
            .unwrap_or_else(|| panic!("PORT triple, got {rest:?}"));
        let rest = rest
            .strip_prefix("[ip_port.rs:871] ")
            .unwrap_or_else(|| panic!("asynStripPath-ed SOURCE, got {rest:?}"));
        let (thread, msg) = rest
            .split_once("] ")
            .unwrap_or_else(|| panic!("THREAD, got {rest:?}"));
        assert!(
            thread.starts_with('[') && thread.matches(',').count() == 2,
            "THREAD is `[name,id,priority]`, got {thread:?}"
        );
        assert_eq!(msg, "read 2 bytes");

        // And no mask label anywhere: C prints none.
        assert!(!line.contains("ERROR"), "no mask label, got {line:?}");
        let _ = std::fs::remove_file(&temp);
    }

    /// C parity: `setTraceIOMask` with `addr >= 0` writes
    /// `pdevice->dpc.trace.traceIOMask`
    /// (asynManager.c:2844-2847). The Rust per-device setter must
    /// place the mask in the `(port, addr)` device slot so the
    /// effective-config resolver returns the device mask when the
    /// emit path supplies the matching `addr`.
    #[test]
    fn set_device_trace_io_mask_writes_device_slot_and_announces() {
        let mgr = TraceManager::new();
        let em = Arc::new(ExceptionManager::new());
        mgr.set_exception_sink(em.clone());
        let observed: Arc<Mutex<Vec<(AsynException, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let obs = observed.clone();
        em.add_callback(move |ev| {
            obs.lock().unwrap().push((ev.exception, ev.addr));
        });

        mgr.register_port("p", true);
        mgr.set_trace_io_mask(Some("p"), TraceIoMask::ASCII);
        mgr.set_device_trace_io_mask("p", 4, TraceIoMask::HEX);

        // Device slot carries the new mask.
        assert_eq!(mgr.snapshot(Some("p"), Some(4)).io_mask, TraceIoMask::HEX);

        // Port dpCommon untouched by the per-device write.
        assert_eq!(mgr.snapshot(Some("p"), None).io_mask, TraceIoMask::ASCII);

        // Per-device announce fires with addr=4.
        let events = observed.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(e, a)| matches!(e, AsynException::TraceIoMask) && *a == 4)
        );
    }

    /// C parity: `setTraceInfoMask` with `pdevice != NULL` writes
    /// `pdevice->dpc.trace.traceInfoMask` and announces per-device
    /// (asynManager.c:2886-2889).
    #[test]
    fn set_device_trace_info_mask_writes_device_slot_and_announces() {
        let mgr = TraceManager::new();
        let em = Arc::new(ExceptionManager::new());
        mgr.set_exception_sink(em.clone());
        let observed: Arc<Mutex<Vec<(AsynException, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let obs = observed.clone();
        em.add_callback(move |ev| {
            obs.lock().unwrap().push((ev.exception, ev.addr));
        });

        mgr.register_port("p", true);
        mgr.set_device_trace_info_mask("p", 2, TraceInfoMask::SOURCE | TraceInfoMask::TIME);

        assert_eq!(
            mgr.snapshot(Some("p"), Some(2)).info_mask,
            TraceInfoMask::SOURCE | TraceInfoMask::TIME
        );

        let events = observed.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(e, a)| matches!(e, AsynException::TraceInfoMask) && *a == 2)
        );
    }

    /// C parity: `setTraceFile` resolves via `findTracePvt(puserPvt)`
    /// which picks the device dpCommon when the asynUser carries a
    /// `pdevice` (asynManager.c:2912-2940). After the per-device
    /// write, `output_device(port, Some(addr), ...)` must emit into
    /// the device-specific sink, not the port-level one.
    #[test]
    fn set_device_trace_file_routes_emit_to_device_sink() {
        let mgr = TraceManager::new();
        let em = Arc::new(ExceptionManager::new());
        mgr.set_exception_sink(em.clone());
        let observed: Arc<Mutex<Vec<(AsynException, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let obs = observed.clone();
        em.add_callback(move |ev| {
            obs.lock().unwrap().push((ev.exception, ev.addr));
        });

        mgr.register_port("p", true);
        mgr.set_trace_mask(Some("p"), TraceMask::ERROR);
        mgr.set_trace_info_mask(Some("p"), TraceInfoMask::PORT);
        mgr.set_device_trace_mask("p", 3, TraceMask::ERROR);
        mgr.set_device_trace_info_mask("p", 3, TraceInfoMask::PORT);

        let dir = tempfile::tempdir().expect("fixture root");
        let port_temp = dir.path().join("asyn_trace_dev_file_port.txt");
        let dev_temp = dir.path().join("asyn_trace_dev_file_dev.txt");
        let port_f = std::fs::File::create(&port_temp).unwrap();
        let dev_f = std::fs::File::create(&dev_temp).unwrap();
        mgr.set_trace_file(Some("p"), TraceFile::File(Arc::new(Mutex::new(port_f))));
        mgr.set_device_trace_file("p", 3, TraceFile::File(Arc::new(Mutex::new(dev_f))));

        mgr.output_device("p", Some(3), 0, TraceMask::ERROR, "device-only-msg");

        let dev_lines = read_lines(&dev_temp);
        assert!(dev_lines.iter().any(|l| l.contains("device-only-msg")));
        let port_lines = read_lines(&port_temp);
        assert!(
            !port_lines.iter().any(|l| l.contains("device-only-msg")),
            "addr-targeted emit must not write to port sink"
        );

        let events = observed.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(e, a)| matches!(e, AsynException::TraceFile) && *a == 3)
        );

        let _ = std::fs::remove_file(&port_temp);
        let _ = std::fs::remove_file(&dev_temp);
    }

    /// R18-63, symptom 1. C `setTraceMask` with `pdevice == NULL` walks
    /// `pport->deviceList` and writes every device's `dpc.trace.traceMask`
    /// before the port's own (asynManager.c:2807-2815). So
    /// `asynSetTraceMask P -1 0x1` after `asynSetTraceMask P 1 0x3f`
    /// **quiets** device 1 — the port-level set is a push-down, not a
    /// lower-priority default the device keeps overriding.
    #[test]
    fn a_port_level_mask_set_pushes_down_and_quiets_a_louder_device() {
        let mgr = TraceManager::new();
        mgr.register_port("P", true);

        // asynSetTraceMask P 1 0x3f
        mgr.set_device_trace_mask("P", 1, TraceMask::ERROR | TraceMask::FLOW);
        assert!(mgr.is_enabled_device("P", 1, TraceMask::FLOW));

        // asynSetTraceMask P -1 0x1
        mgr.set_trace_mask(Some("P"), TraceMask::ERROR);

        assert!(
            !mgr.is_enabled_device("P", 1, TraceMask::FLOW),
            "the port-level set must overwrite device 1's slot"
        );
        assert!(mgr.is_enabled_device("P", 1, TraceMask::ERROR));
    }

    /// R18-63, symptom 2. C's `findTracePvt` reaches `pasynBase->trace` only
    /// for an asynUser with no port at all (asynManager.c:548-553). Every
    /// output path here names a port, so a global `set_trace_mask(None, ..)`
    /// must not become the effective mask of a port that never asked for it:
    /// the port stays on its `tracePvtInit` birth value (:456).
    #[test]
    fn a_global_mask_set_does_not_reach_a_port() {
        let mgr = TraceManager::new();
        mgr.register_port("Q", false);

        mgr.set_trace_mask(None, TraceMask::ERROR | TraceMask::FLOW);

        assert!(
            !mgr.is_enabled("Q", TraceMask::FLOW),
            "a global set must not appear on a port's dpCommon"
        );
        assert!(mgr.is_enabled("Q", TraceMask::ERROR), "born with ERROR");
        // And the global slot really did move — this is a routing test, not
        // a no-op test.
        assert!(mgr.get_trace_mask(None).contains(TraceMask::FLOW));
    }

    /// R18-63, symptom 3. `locateDevice` returns NULL unless the port
    /// carries `ASYN_MULTIDEVICE` (asynManager.c:576), so `connectDevice`
    /// leaves `puserPvt->pdevice` NULL on a single-device port and both the
    /// reads and the writes land on the port's own `dpCommon`. An addr on
    /// such a port names no device at all.
    #[test]
    fn an_address_on_a_single_device_port_names_the_port_not_a_device() {
        let mgr = TraceManager::new();
        mgr.register_port("S", false);

        // A device-scoped write on a single-device port IS the port write.
        mgr.set_device_trace_mask("S", 3, TraceMask::ERROR | TraceMask::FLOW);
        assert!(
            mgr.is_enabled("S", TraceMask::FLOW),
            "the write landed on the port, as C's NULL pdevice makes it"
        );

        // ...and a later port write is seen at that address, because there
        // is no device slot holding a stale louder value.
        mgr.set_trace_mask(Some("S"), TraceMask::ERROR);
        assert!(!mgr.is_enabled_device("S", 3, TraceMask::FLOW));

        // The same address on a MULTIDEVICE port does name a device.
        mgr.register_port("M", true);
        mgr.set_trace_mask(Some("M"), TraceMask::ERROR);
        mgr.set_device_trace_mask("M", 3, TraceMask::ERROR | TraceMask::FLOW);
        assert!(mgr.is_enabled_device("M", 3, TraceMask::FLOW));
        assert!(
            !mgr.is_enabled("M", TraceMask::FLOW),
            "and the port's own slot is untouched by it"
        );
    }

    /// R18-63, the announce half. C announces once per device it wrote and
    /// then once for the port (asynManager.c:2795-2800), which is what keeps
    /// every `asynRecord` attached to a device refreshing its `TMSK` after a
    /// port-level `asynSetTraceMask`.
    #[test]
    fn a_port_level_set_announces_per_device_then_for_the_port() {
        let mgr = TraceManager::new();
        let em = Arc::new(ExceptionManager::new());
        mgr.set_exception_sink(em.clone());
        mgr.register_port("P", true);
        mgr.set_device_trace_mask("P", 1, TraceMask::ERROR);
        mgr.set_device_trace_mask("P", 2, TraceMask::ERROR);

        let observed: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
        let obs = observed.clone();
        em.add_callback(move |ev| {
            if matches!(ev.exception, AsynException::TraceMask) {
                obs.lock().unwrap().push(ev.addr);
            }
        });

        mgr.set_trace_mask(Some("P"), TraceMask::ERROR | TraceMask::FLOW);

        assert_eq!(*observed.lock().unwrap(), vec![1, 2, -1]);
    }

    #[test]
    fn every_mask_mutator_opens_the_lock_free_gate() {
        let mgr = TraceManager::new();
        mgr.register_port("G", true);
        assert!(!mgr.is_enabled_device("G", 1, TraceMask::IO_DEVICE));
        mgr.set_device_trace_mask("G", 1, TraceMask::IO_DEVICE);
        assert!(mgr.is_enabled_device("G", 1, TraceMask::IO_DEVICE));
        mgr.set_trace_mask(Some("G"), TraceMask::IO_DRIVER);
        assert!(mgr.is_enabled_device("G", 1, TraceMask::IO_DRIVER));
        // The gate stays open for a bit no slot holds now; the slot decides.
        assert!(!mgr.is_enabled_device("G", 1, TraceMask::IO_DEVICE));
        mgr.set_trace_mask(None, TraceMask::FLOW);
        assert!(mgr.may_trace(TraceMask::FLOW));
    }
}
