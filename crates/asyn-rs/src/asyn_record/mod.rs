pub mod registry;
pub use registry::{
    PortEntry, PortRegistry, asyn_record_factory, get_port, register_asyn_record_type,
    register_port,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, FieldDesc, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::error::AsynResult;
use crate::exception::{AsynException, ExceptionCallbackId, ExceptionManager};
use crate::interpose::EomReason;
use crate::port_handle::PortHandle;
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use crate::user::AsynUser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum TransferMode {
    WriteRead = 0,
    Write = 1,
    Read = 2,
    Flush = 3,
    NoIo = 4,
}

impl TransferMode {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::WriteRead,
            1 => Self::Write,
            2 => Self::Read,
            3 => Self::Flush,
            4 => Self::NoIo,
            _ => Self::WriteRead,
        }
    }
}

// ===== Interface Type =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum InterfaceType {
    Octet = 0,
    Int32 = 1,
    UInt32Digital = 2,
    Float64 = 3,
}

impl InterfaceType {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Octet,
            1 => Self::Int32,
            2 => Self::UInt32Digital,
            3 => Self::Float64,
            _ => Self::Octet,
        }
    }
}

// ===== Baud rate menu mapping =====

/// Map a baud rate integer to the serialBAUD menu index.
fn baud_rate_to_menu_index(baud: i32) -> i32 {
    match baud {
        300 => 1,
        600 => 2,
        1200 => 3,
        2400 => 4,
        4800 => 5,
        9600 => 6,
        19200 => 7,
        38400 => 8,
        57600 => 9,
        115200 => 10,
        230400 => 11,
        460800 => 12,
        576000 => 13,
        921600 => 14,
        1152000 => 15,
        _ => 0, // Unknown
    }
}

/// Map a serialBAUD menu index to a baud rate integer.
fn menu_index_to_baud_rate(idx: i32) -> i32 {
    match idx {
        1 => 300,
        2 => 600,
        3 => 1200,
        4 => 2400,
        5 => 4800,
        6 => 9600,
        7 => 19200,
        8 => 38400,
        9 => 57600,
        10 => 115200,
        11 => 230400,
        12 => 460800,
        13 => 576000,
        14 => 921600,
        15 => 1152000,
        _ => 0,
    }
}

/// `asynFMT` menu value for ASCII I/O format (`asynFMT_ASCII` in EPICS
/// `asynRecord.dbd`). On output C escape-translates AOUT; on input the
/// read destination is the AINP string (`asynRecord.c:1486`,`:1503-1509`).
const ASYN_FMT_ASCII: i32 = 0;

/// `asynFMT` menu value for Hybrid I/O format (`asynFMT_Hybrid`). On
/// output C escape-translates the binary BOUT buffer read as a C string;
/// on input the read destination is the BINP byte buffer
/// (`asynRecord.c:1491-1495`,`:1507-1510`).
const ASYN_FMT_HYBRID: i32 = 1;

/// `asynFMT` menu value for binary I/O format (`asynFMT_Binary`). Selects
/// the raw BOUT / BINP byte buffers over the ASCII AOUT / AINP strings and
/// suppresses escape translation (`asynRecord.c:1496-1502`).
const ASYN_FMT_BINARY: i32 = 2;

/// Capacity of the `AINP` input string, in bytes — `sizeof(pasynRec->ainp)`.
/// `AINP` is a `DBF_STRING` (`asynRecord.dbd:261`), i.e. EPICS
/// `MAX_STRING_SIZE` = 40 including the NUL terminator. C `performOctetIO`
/// sizes the ASCII read by exactly this (`asynRecord.c:1505-1506`
/// `inlen = sizeof(pasynRec->ainp)`) and keys the ASCII overflow on it
/// (`:1602-1608`), independent of `IMAX` — only Hybrid and Binary read into the
/// `IMAX`-sized `BINP` buffer.
const AINP_SIZE: usize = 40;

/// Decode a C-style backslash-escaped string into the raw bytes the
/// driver layer expects. Mirrors EPICS base's `dbTranslateEscape`
/// (`libCom/misc/dbTranslateEscape.c`) — supports the standard
/// escape sequences `\r \n \t \\ \" \0` plus octal `\NNN`. Used by
/// asynRecord OEOS/IEOS writes (C asynRecord.c:374-393) and ASCII octet
/// output (`:1489`) so a configured `\r\n` reaches the driver as the two
/// raw bytes `0x0D 0x0A`, not the four-byte literal. Also used by the
/// `asynOctetCmdResponse` factory to escape the literal command (C
/// `initCmdBuffer`, devAsynOctet.c) once at build time.
pub(crate) fn translate_escape(s: &str) -> Vec<u8> {
    translate_escape_bytes(s.as_bytes())
}

/// Byte-oriented core of [`translate_escape`]. C `dbTranslateEscape`
/// operates on a NUL-terminated C string regardless of encoding; the
/// Hybrid octet output path (`asynRecord.c:1494`) feeds it the raw binary
/// BOUT buffer, so the translator must accept arbitrary bytes.
fn translate_escape_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.iter().copied().peekable();
    while let Some(c) = chars.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            // dangling backslash at end of input → pass through, C
            // `dbTranslateEscape` returns input length on incomplete
            // escape so we match (push the literal `\`).
            out.push(b'\\');
            break;
        };
        let decoded = match next {
            b'r' => 0x0D,
            b'n' => 0x0A,
            b't' => 0x09,
            b'\\' => b'\\',
            b'"' => b'"',
            b'\'' => b'\'',
            b'0'..=b'7' => {
                // Octal escape `\N`, `\NN`, `\NNN` — C `dbTranslateEscape`
                // consumes up to three octal digits. `\0` with no further
                // digits still decodes to NUL.
                let mut val = u32::from(next - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(&d) if (b'0'..=b'7').contains(&d) => {
                            val = val * 8 + u32::from(d - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                out.push((val & 0xFF) as u8);
                continue;
            }
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0C,
            b'v' => 0x0B,
            other => {
                // Unknown escape — pass through literally (C does the
                // same for unrecognized backslash sequences).
                out.push(b'\\');
                out.push(other);
                continue;
            }
        };
        out.push(decoded);
    }
    out
}

/// Resolve an asynRecord `TFIL` string to its trace sink, C-faithful per
/// `asynRecord.c:453-468`: empty and `<stdout>` -> stdout, `<stderr>` ->
/// stderr, `<errlog>` -> the errlog sink. Any other value is a file path
/// opened with append semantics (`fopen(.., "a+")`) so an existing trace
/// log is preserved rather than truncated. The bracketed tokens are the
/// only special names — a bare `"stdout"`/`"stderr"` is a literal filename,
/// exactly as in C.
fn open_trace_file(tfil: &str) -> std::io::Result<TraceFile> {
    match tfil {
        "" | "<stdout>" => Ok(TraceFile::Stdout),
        "<stderr>" => Ok(TraceFile::Stderr),
        "<errlog>" => Ok(TraceFile::Errlog),
        path => std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map(|f| TraceFile::File(Arc::new(std::sync::Mutex::new(f)))),
    }
}

// ===== AsynRecord =====

/// Full asynRecord with all 67 fields.
pub struct AsynRecord {
    // --- Address fields ---
    pub port: String,
    pub addr: i32,
    pub pcnct: i32, // Port Connect/Disconnect (menu: 0=Disconnect, 1=Connect)
    pub drvinfo: String,
    pub reason: i32,

    // --- I/O control ---
    pub tmod: i32,     // Transfer mode (menu asynTMOD)
    pub tmot: f64,     // Timeout (sec)
    pub iface: i32,    // Interface (menu asynINTERFACE)
    pub octetiv: i32,  // asynOctet is valid
    pub optioniv: i32, // asynOption is valid
    pub gpibiv: i32,   // asynGPIB is valid
    pub i32iv: i32,    // asynInt32 is valid
    pub ui32iv: i32,   // asynUInt32Digital is valid
    pub f64iv: i32,    // asynFloat64 is valid

    // --- asynOctet output ---
    pub aout: String,
    pub oeos: String,
    pub bout: Vec<u8>,
    pub omax: i32,
    pub nowt: i32,
    pub nawt: i32,
    pub ofmt: i32, // Output format (menu asynFMT)

    // --- asynOctet input ---
    pub ainp: String,
    pub tinp: String,
    pub ieos: String,
    pub binp: Vec<u8>,
    pub imax: i32,
    pub nrrd: i32,
    pub nord: i32,
    pub ifmt: i32, // Input format (menu asynFMT)
    pub eomr: i32, // EOM reason

    // --- Int32/UInt32/Float64 data ---
    pub i32inp: i32,
    pub i32out: i32,
    pub ui32inp: u32,
    pub ui32out: u32,
    pub ui32mask: u32,
    pub f64inp: f64,
    pub f64out: f64,

    // --- Serial control ---
    pub baud: i32,
    pub lbaud: i32,
    pub prty: i32,
    pub dbit: i32,
    pub sbit: i32,
    pub mctl: i32,
    pub fctl: i32,
    pub ixon: i32,
    pub ixoff: i32,
    pub ixany: i32,

    // --- IP options ---
    pub hostinfo: String,
    pub drto: i32,

    // --- GPIB ---
    pub ucmd: i32,
    pub acmd: i32,
    pub spr: i32,

    // --- Trace control ---
    pub tmsk: i32,
    pub tb0: i32,
    pub tb1: i32,
    pub tb2: i32,
    pub tb3: i32,
    pub tb4: i32,
    pub tb5: i32,
    pub tiom: i32,
    pub tib0: i32,
    pub tib1: i32,
    pub tib2: i32,
    pub tinm: i32,
    pub tinb0: i32,
    pub tinb1: i32,
    pub tinb2: i32,
    pub tinb3: i32,
    pub tsiz: i32,
    pub tfil: String,

    // --- Connection management ---
    pub auct: i32, // Autoconnect (menu: 0=noAutoConnect, 1=autoConnect)
    pub cnct: i32, // Connect/Disconnect (menu: 0=Disconnect, 1=Connect)
    pub enbl: i32, // Enable/Disable (menu: 0=Disable, 1=Enable)

    // --- Misc ---
    pub val: i32,
    pub errs: String,
    pub aqr: i32,

    // --- Runtime state (not EPICS fields) ---
    port_entry: Option<PortEntry>,
    resolved_reason: usize,

    // The record's canonical name plus a cycle-free handle to its own
    // database, handed over by the framework at `add_record` via
    // `set_async_context` (C records reach the IOC the same way at
    // `dbDefineRecord`). Lets `process()` run port I/O off the scan thread
    // and re-enter on completion, and lets the trace exception callback post
    // readback fields out-of-band — neither of which the async callback can
    // do by mutating the framework-owned record directly.
    async_ctx: Option<(String, AsyncDbHandle)>,
    // A non-blocking port request that is queued / in flight. `Some` exactly
    // while `process()` has returned `AsyncPending` and the off-thread
    // orchestration has not yet re-entered (C `stateIO`, asynRecord.c:216).
    // Holds the shared `CancelToken` (the asynRecord `AQR`/`cancelRequest`
    // target) and the slot the orchestration fills before it fires the
    // completion re-entry.
    io_inflight: Option<IoInFlight>,

    // Set by the trace exception callback (C `exceptCallback`,
    // asynRecord.c:903-917) when an external `setTrace{Mask,IOMask,
    // InfoMask}` fires; consumed by `process()`, which re-imports the
    // live masks through `read_trace_state`. The async callback cannot
    // mutate the record (it is owned by the record framework) or post
    // monitors, so the refresh is deferred to the next process cycle.
    trace_status_dirty: Arc<AtomicBool>,
    // Owner handle for the registered trace exception callback so it can
    // be removed on disconnect / drop (C `exceptionCallbackRemove`,
    // asynRecord.c:523,1154,1313).
    trace_except_cb: Option<(Arc<ExceptionManager>, ExceptionCallbackId)>,
    // The record alarm `(stat, sevr)` raised by this process cycle's I/O —
    // the C `recGblSetSevr(pasynRec, …)` calls in `performIO`
    // (asynRecord.c:1380-1621) and the no-asynGpib-interface COMM alarm
    // (:1649/:1695). Set by `apply_io_outcome` / the UCMD-ACMD short-circuits;
    // the single consumer is `check_alarms`, which `take()`s it and commits it
    // via `rec_gbl_set_sevr`. Every complete-returning process cycle reaches
    // `check_alarms`, so the `take()` is the single clear point — no path
    // stages an alarm and then bypasses the consumer.
    io_alarm: Option<(u16, AlarmSeverity)>,
}

impl Default for AsynRecord {
    fn default() -> Self {
        Self {
            port: String::new(),
            addr: 0,
            pcnct: 0,
            drvinfo: String::new(),
            reason: 0,
            tmod: 0,
            tmot: 1.0,
            iface: 0,
            octetiv: 0,
            optioniv: 0,
            gpibiv: 0,
            i32iv: 0,
            ui32iv: 0,
            f64iv: 0,
            aout: String::new(),
            oeos: String::new(),
            bout: Vec::new(),
            omax: 80,
            nowt: 80,
            nawt: 0,
            ofmt: 0,
            ainp: String::new(),
            tinp: String::new(),
            ieos: String::new(),
            binp: Vec::new(),
            imax: 80,
            nrrd: 0,
            nord: 0,
            ifmt: 0,
            eomr: 0,
            i32inp: 0,
            i32out: 0,
            ui32inp: 0,
            ui32out: 0,
            ui32mask: 0xFFFFFFFF,
            f64inp: 0.0,
            f64out: 0.0,
            baud: 0,
            lbaud: 0,
            prty: 0,
            dbit: 0,
            sbit: 0,
            mctl: 0,
            fctl: 0,
            ixon: 0,
            ixoff: 0,
            ixany: 0,
            hostinfo: String::new(),
            drto: 0,
            ucmd: 0,
            acmd: 0,
            spr: 0,
            tmsk: 0,
            tb0: 0,
            tb1: 0,
            tb2: 0,
            tb3: 0,
            tb4: 0,
            tb5: 0,
            tiom: 0,
            tib0: 0,
            tib1: 0,
            tib2: 0,
            tinm: 0,
            tinb0: 0,
            tinb1: 0,
            tinb2: 0,
            tinb3: 0,
            tsiz: 80,
            tfil: String::new(),
            auct: 1,
            cnct: 0,
            enbl: 1,
            val: 0,
            errs: String::new(),
            aqr: 0,
            port_entry: None,
            resolved_reason: 0,
            async_ctx: None,
            io_inflight: None,
            trace_status_dirty: Arc::new(AtomicBool::new(false)),
            trace_except_cb: None,
            io_alarm: None,
        }
    }
}

// ===== Non-blocking I/O orchestration =====
//
// C asynRecord queues `performIO` as one request (`queueRequest`,
// asynRecord.c:342) that the port thread runs in `asynCallbackProcess`
// (asynRecord.c:808-832); `process()` returns with `pact=TRUE` and the
// record completes on the callback's `callbackRequestProcessCallback`
// re-process. The Rust analogue runs `performIO` off the scan thread in a
// spawned task and re-enters `process()` via the PACT async-record
// primitive when the port I/O finishes.

/// A non-blocking port request that is queued / in flight.
struct IoInFlight {
    /// Shared with the orchestration task. Cancelling it makes the actor drop
    /// the request at its cancel check (a still-queued phase is removed,
    /// C `cancelRequest` `wasQueued==true`, asynManager.c:1630-1666), so
    /// `run_io_plan` records the `AQR` "I/O request canceled" outcome. An
    /// `AQR` write (asynRecord.c:393-408) sets this; the completion re-entry
    /// then applies the cancel and finishes the record.
    cancel: CancelToken,
    /// The orchestration writes the completed `IoOutcome` here before it
    /// fires the re-entry, so the completion `process()` can apply it.
    result: Arc<Mutex<Option<IoOutcome>>>,
}

/// Immutable snapshot of the record fields `performIO` reads, built on the
/// scan thread so the off-thread orchestration never touches the record.
struct IoPlan {
    tmod: TransferMode,
    iface: InterfaceType,
    // Per-request `asynUser` inputs. Stored as primitives (not a built
    // `AsynUser`) because `AsynUser` owns a non-`Clone` `user_data` box, and
    // every phase consumes a fresh user by value (`io_user`/`flush_user`).
    reason: usize,
    addr: i32,
    timeout: std::time::Duration,
    // Write inputs (`performOctetIO`/`performGPIBIO`, asynRecord.c:1470+).
    octet_out: Vec<u8>,
    octet_out_len: usize,
    ofmt: i32,
    i32out: i32,
    ui32out: u32,
    ui32mask: u32,
    f64out: f64,
    // Read inputs.
    octet_buf_size: usize,
    // C `performOctetIO`'s `inlen` (asynRecord.c:1503-1511): the capacity of the
    // IFMT-selected input buffer — `sizeof(ainp)` (= AINP_SIZE) for ASCII, IMAX
    // for Hybrid/Binary, which read into BINP. It is both the default read
    // length and the overflow threshold (`:1602-1620`); `octet_buf_size` is the
    // per-request read length (`min(NRRD, inlen)` or `inlen`), so an
    // NRRD-limited short read can never reach `in_len` and never reads as
    // overflow — the same reason C's check is independent of NRRD.
    in_len: usize,
    ifmt: i32,
}

/// The record fields `performIO` writes from the I/O results. Each `Some`
/// is applied to the record on the completion re-entry; `None` leaves the
/// field untouched (the phase did not run / produced no value).
#[derive(Default)]
struct IoOutcome {
    nawt: Option<i32>,
    eomr: Option<i32>,
    nord: Option<i32>,
    tinp: Option<String>,
    ainp: Option<String>,
    binp: Option<Vec<u8>>,
    i32inp: Option<i32>,
    ui32inp: Option<u32>,
    f64inp: Option<f64>,
    errs: Option<String>,
    /// Record alarm `(stat, sevr)` this I/O cycle raises — the C
    /// `recGblSetSevr(pasynRec, …)` calls scattered through `performIO`
    /// (asynRecord.c:1380-1621). Accumulated with the "raise only if strictly
    /// higher" rule via [`raise_io_alarm`] so a write-then-read `Write_Read`
    /// keeps the first equal-severity stat, exactly as C's repeated
    /// `recGblSetSevr` does. Applied to the record in [`AsynRecord::apply_io_outcome`]
    /// and committed by [`AsynRecord::check_alarms`].
    alarm: Option<(u16, AlarmSeverity)>,
}

/// Raise `(stat, sevr)` into an [`IoOutcome`] mirroring `recGblSetSevr`
/// (recGbl.c:258, [`rec_gbl_set_sevr`]): a new severity replaces the pending
/// alarm only when it is **strictly higher**, so an equal-severity later call
/// keeps the earlier `stat`. C's `performIO` relies on this when both the
/// write and read phase of a `Write_Read` fail at MAJOR — the write's stat is
/// the one that survives.
fn raise_io_alarm(out: &mut IoOutcome, stat: u16, sevr: AlarmSeverity) {
    let higher = match out.alarm {
        Some((_, cur)) => (sevr as u16) > (cur as u16),
        None => true,
    };
    if higher {
        out.alarm = Some((stat, sevr));
    }
}

/// Build the per-transfer `asynUser` for a write/read phase. C `asynRecord.c`
/// sets `pasynUser->timeout = precord->tmot` and the parameter reason/addr
/// before every transfer. A fresh user per submit (the actor consumes it by
/// value) and the plan's snapshot keep the off-thread orchestration off the
/// record.
fn io_user(plan: &IoPlan) -> AsynUser {
    AsynUser::new(plan.reason)
        .with_addr(plan.addr)
        .with_timeout(plan.timeout)
}

/// Build the `asynUser` for a `Flush` phase. C `asynRecord.c` issues the flush
/// with the same reason/addr but no transfer timeout.
fn flush_user(plan: &IoPlan) -> AsynUser {
    AsynUser::new(plan.reason).with_addr(plan.addr)
}

/// One phase of a `performIO` cycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IoPhase {
    Flush,
    Write,
    Read,
}

/// The phases one `performIO` cycle runs, in C's execution order — the single
/// owner of "which phases, and when" for both the synchronous
/// [`AsynRecord::perform_io`] and the off-thread [`run_io_plan`], so the two
/// cannot drift apart.
///
/// C `performOctetIO` (asynRecord.c:1518-1558):
///
/// 1. **Flush, before the write**, for `TMOD == Flush` *and* `TMOD ==
///    Write_Read` (`:1518-1520`). Write/Read is the default TMOD, and this is
///    the whole point of the flush: drop bytes left in the driver from a
///    previous transaction so they cannot be prepended to the fresh response.
///    Running it after the read — or only for `TMOD == Flush` — leaves exactly
///    the stale-byte framing error the flush exists to prevent.
/// 2. Write, for `Write` / `Write_Read` (`:1524`).
/// 3. Read, for `Read` / `Write_Read` (`:1557`).
///
/// The flush is octet-only: it lives inside `performOctetIO`, and the register
/// handlers (`performInt32IO` :1370-1395, `performUInt32DigitalIO` :1405-1433,
/// `performFloat64IO` :1442-1467) have no flush branch — a `TMOD=Flush` cycle on
/// a register interface does nothing in C.
fn io_phases(plan: &IoPlan) -> Vec<IoPhase> {
    let mut phases = Vec::with_capacity(3);
    if plan.iface == InterfaceType::Octet
        && matches!(plan.tmod, TransferMode::Flush | TransferMode::WriteRead)
    {
        phases.push(IoPhase::Flush);
    }
    if matches!(plan.tmod, TransferMode::Write | TransferMode::WriteRead) {
        phases.push(IoPhase::Write);
    }
    if matches!(plan.tmod, TransferMode::Read | TransferMode::WriteRead) {
        phases.push(IoPhase::Read);
    }
    phases
}

/// Build the write-phase `RequestOp` for an interface. C `performOctetIO`
/// (asynRecord.c:1528-1546) suppresses the driver's output EOS for a binary
/// write; `OctetWriteBinary` brackets that save/clear/restore in the actor.
fn io_write_op(plan: &IoPlan) -> RequestOp {
    match plan.iface {
        InterfaceType::Octet => {
            if plan.ofmt == ASYN_FMT_BINARY {
                RequestOp::OctetWriteBinary {
                    data: plan.octet_out.clone(),
                }
            } else {
                RequestOp::OctetWrite {
                    data: plan.octet_out.clone(),
                }
            }
        }
        InterfaceType::Int32 => RequestOp::Int32Write { value: plan.i32out },
        InterfaceType::UInt32Digital => RequestOp::UInt32DigitalWrite {
            value: plan.ui32out,
            mask: plan.ui32mask,
        },
        InterfaceType::Float64 => RequestOp::Float64Write { value: plan.f64out },
    }
}

/// Build the read-phase `RequestOp` for an interface. C `performOctetIO`
/// (asynRecord.c:1564-1581) suppresses the driver's input EOS for a binary
/// read; `OctetReadBinary` brackets that in the actor.
fn io_read_op(plan: &IoPlan) -> RequestOp {
    match plan.iface {
        InterfaceType::Octet => {
            if plan.ifmt == ASYN_FMT_BINARY {
                RequestOp::OctetReadBinary {
                    buf_size: plan.octet_buf_size,
                }
            } else {
                RequestOp::OctetRead {
                    buf_size: plan.octet_buf_size,
                }
            }
        }
        InterfaceType::Int32 => RequestOp::Int32Read,
        InterfaceType::UInt32Digital => RequestOp::UInt32DigitalRead {
            mask: plan.ui32mask,
        },
        InterfaceType::Float64 => RequestOp::Float64Read,
    }
}

/// Record a write-phase result into the outcome — the C `performIO` write
/// branch (asynRecord.c:1524-1556 octet, :1442-1453 register). A write
/// failure reports `ERRS` but, like C, does not skip the read phase.
fn record_write_result(plan: &IoPlan, out: &mut IoOutcome, res: AsynResult<RequestResult>) {
    if plan.iface != InterfaceType::Octet {
        // C performInt32IO/performUInt32DigitalIO/performFloat64IO write error
        // -> reportError + recGblSetSevr(WRITE_ALARM, MAJOR)
        // (asynRecord.c:1377-1381 / 1413-1417 / 1449-1453). These handlers have
        // no byte count, so they never touch NAWT.
        if let Err(e) = res {
            out.errs = Some(format!("write: {e}"));
            raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
        }
        return;
    }

    // C performOctetIO write branch (asynRecord.c:1524-1556). `nbytesTransfered`
    // is what the *device* took: C seeds it to 0 (:1526), the octet chain writes
    // it out on success and on failure alike, and the record commits it —
    // `nawt = nbytesTransfered` (:1547) runs *before* the status test at :1551.
    // So both arms report a real count: the reply's `nbytes` on success, and the
    // failure's `PartialWrite` carrier on error (absent => the layer moved
    // nothing, which is exactly C's untouched seed of 0).
    let (nawt, err) = match res {
        Ok(result) => (result.nbytes, None),
        Err(e) => (e.partial_write().unwrap_or(0), Some(e)),
    };
    out.nawt = Some(nawt as i32);

    // C :1551-1555 — "Something is wrong if we couldn't write everything": the
    // diagnostic fires on a failing status *or* a short write that reported
    // success, and lands in ERRS via reportError (:2028-2048). The octet write
    // branch raises NO record severity — only the message — so unlike the
    // register writes above it sets no alarm.
    if err.is_some() || nawt != plan.octet_out_len {
        let detail = match &err {
            Some(e) => e.to_string(),
            None => format!("wrote {} of {} chars", nawt, plan.octet_out_len),
        };
        out.errs = Some(format!("Write error, nout={nawt}, {detail}"));
    }
}

/// Record a read-phase result into the outcome — the C `performIO` read
/// branch (asynRecord.c:1557-1631 octet, :1478-1527 register).
fn record_read_result(plan: &IoPlan, out: &mut IoOutcome, res: AsynResult<RequestResult>) {
    match plan.iface {
        InterfaceType::Octet => {
            // C performOctetIO reads into a buffer it memset to zero
            // (asynRecord.c:1560) and then assigns EOMR / the IFMT-selected
            // input field / NORD / TINP from `nbytesTransfered` **regardless
            // of the returned status** (:1591-1629) — the error branch at
            // :1583 reports the error and raises the alarm but does not skip
            // the assignments. So a device that emits a partial line and then
            // goes quiet lands AINP="abc", NORD=3 *and* a READ_ALARM.
            //
            // Rust splits the transfer from the status across `Result`, so
            // recover the transfer from both arms first (an error that moved
            // bytes carries them in `AsynError::PartialRead`; one that moved
            // none reports a zero transfer, which is exactly C's memset
            // buffer) and run the one assignment tail below. Register reads
            // keep their stale-on-error fields, matching their C handlers
            // which never memset.
            let (data, eom, err) = match res {
                Ok(result) => (
                    result.data.unwrap_or_default(),
                    result.eom_reason,
                    None::<crate::error::AsynError>,
                ),
                Err(e) => {
                    let (data, eom) = match e.partial_read() {
                        Some(p) => (p.data.clone(), p.eom_reason.bits()),
                        None => (Vec::new(), 0),
                    };
                    (data, eom, Some(e))
                }
            };

            out.eomr = Some(eom as i32);
            // NORD is the raw transfer count (C `nord = nbytesTransfered`,
            // asynRecord.c:1627) — independent of the overflow
            // NUL-truncation below and of UTF-8-lossy expansion.
            out.nord = Some(data.len() as i32);
            // C performOctetIO's overflow tests (asynRecord.c:1602-1621) are
            // plain length compares against the IFMT-selected buffer capacity
            // `inlen`, with no end-of-message condition: a read that filled the
            // buffer is an overflow even when the driver reported EOS/END. Each
            // raises a MINOR READ alarm; ASCII and Hybrid also NUL-terminate the
            // buffer at its last byte. An NRRD-limited read can never reach
            // `in_len` (it is capped by it), so it cannot false-positive.
            // Binary compares with `>` ("should not happen") — so it never fires
            // in practice, since the driver cannot return more than requested.
            let overflow = match plan.ifmt {
                ASYN_FMT_BINARY => data.len() > plan.in_len,
                _ => plan.in_len > 0 && data.len() >= plan.in_len,
            };
            // C stores the device bytes into the single IFMT-selected
            // field (ASCII -> AINP, Binary/Hybrid -> BINP,
            // asynRecord.c:1503-1509) and posts TINP (escaped) for
            // every read mode.
            out.tinp = Some(crate::trace::format_io_data(&data, TraceIoMask::ESCAPE));
            if plan.ifmt == ASYN_FMT_ASCII {
                // On overflow C terminates AINP at the buffer end
                // (`inptr[sizeof(ainp)-1] = '\0'`, :1608) — drop the
                // final byte so the string leaves room for the
                // conceptual terminator.
                let bytes = if overflow {
                    &data[..plan.in_len - 1]
                } else {
                    &data[..]
                };
                out.ainp = Some(String::from_utf8_lossy(bytes).to_string());
            } else {
                let mut data = data;
                // Hybrid overflow: C writes the NUL into the BINP buffer itself
                // (`inptr[imax - 1] = '\0'`, :1615). Binary is left untouched
                // (:1616-1621 reports but does not terminate).
                if overflow && plan.ifmt == ASYN_FMT_HYBRID {
                    data[plan.in_len - 1] = 0;
                }
                out.binp = Some(data);
            }
            if overflow {
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Minor);
            }
            if let Some(e) = err {
                out.errs = Some(format!("read: {e}"));
                // C performOctetIO read error -> recGblSetSevr(READ_ALARM,
                // MAJOR) (asynRecord.c:1599).
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        }
        InterfaceType::Int32 => match res {
            Ok(result) => match result.int_val {
                Some(v) => out.i32inp = Some(v),
                None => out.errs = Some("read: int32 read returned no value".to_string()),
            },
            // C performInt32IO read error -> recGblSetSevr(READ_ALARM, MAJOR)
            // (asynRecord.c:1393).
            Err(e) => {
                out.errs = Some(format!("read: {e}"));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
        InterfaceType::UInt32Digital => match res {
            Ok(result) => {
                if let Some(v) = result.uint_val {
                    out.ui32inp = Some(v);
                }
            }
            // C performUInt32DigitalIO read error -> recGblSetSevr(READ_ALARM,
            // MAJOR) (asynRecord.c:1431).
            Err(e) => {
                out.errs = Some(format!("read: {e}"));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
        InterfaceType::Float64 => match res {
            Ok(result) => match result.float_val {
                Some(v) => out.f64inp = Some(v),
                None => out.errs = Some("read: float64 read returned no value".to_string()),
            },
            // C performFloat64IO read error -> recGblSetSevr(READ_ALARM, MAJOR)
            // (asynRecord.c:1465).
            Err(e) => {
                out.errs = Some(format!("read: {e}"));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
    }
}

/// Record one phase's result into the outcome — the phase→recorder mapping,
/// shared by the synchronous and off-thread runners so a phase cannot be handled
/// differently on the two paths. As in C `performIO`, a failed phase reports but
/// does not skip the phases after it.
fn record_phase_result(
    plan: &IoPlan,
    out: &mut IoOutcome,
    phase: IoPhase,
    res: AsynResult<RequestResult>,
) {
    match phase {
        IoPhase::Flush => {
            if let Err(e) = res {
                out.errs = Some(format!("flush: {e}"));
            }
        }
        IoPhase::Write => record_write_result(plan, out, res),
        IoPhase::Read => record_read_result(plan, out, res),
    }
}

/// Run `performIO`'s flush/write/read phases off the scan thread against the
/// port actor, threading the shared `CancelToken` so an `AQR`/`cancelRequest`
/// (asynManager.c:1630) aborts a still-queued phase. Mirrors the synchronous
/// [`AsynRecord::perform_io`] phase order; both feed
/// [`AsynRecord::apply_io_outcome`], so the field mapping lives in one place.
///
/// A cancelled phase short-circuits with the C `cancelRequest` "I/O request
/// canceled" message (asynRecord.c:398); other errors are recorded but, as in
/// C `performIO`, do not skip the remaining phases.
async fn run_io_plan(handle: PortHandle, plan: IoPlan, cancel: CancelToken) -> IoOutcome {
    let mut out = IoOutcome::default();

    if cancel.is_cancelled() {
        out.errs = Some(CANCELED_MSG.to_string());
        return out;
    }

    for phase in io_phases(&plan) {
        let res = match phase {
            IoPhase::Flush => {
                handle
                    .submit_cancellable(RequestOp::Flush, flush_user(&plan), cancel.clone())
                    .await
            }
            IoPhase::Write => {
                handle
                    .submit_cancellable(io_write_op(&plan), io_user(&plan), cancel.clone())
                    .await
            }
            IoPhase::Read => {
                handle
                    .submit_cancellable(io_read_op(&plan), io_user(&plan), cancel.clone())
                    .await
            }
        };
        if cancel.is_cancelled() {
            out.errs = Some(CANCELED_MSG.to_string());
            return out;
        }
        record_phase_result(&plan, &mut out, phase, res);
    }

    out
}

/// C `reportError(pasynRec, status, "I/O request canceled")` for a dequeued
/// `AQR` request (asynRecord.c:398).
const CANCELED_MSG: &str = "I/O request canceled";

/// Build the asyn trace readback fields from the three trace masks — the
/// single source of the mask→field mapping that C `monitorStatus`
/// recomputes and posts (asynRecord.c:1066-1117). The bit assignments mirror
/// [`AsynRecord::update_trace_bits_from_mask`] /
/// [`AsynRecord::update_io_bits_from_mask`] /
/// [`AsynRecord::update_info_bits_from_mask`] and reference the same
/// `Trace*Mask` constants, so the out-of-band trace post and the
/// `process()`-path re-import (`read_trace_state`) cannot diverge. Field DBF
/// types match `get_field`: the mask fields are `Long`, the bit fields
/// `Short`.
fn trace_readback_fields(
    trace_mask: u32,
    io_mask: u32,
    info_mask: u32,
) -> Vec<(String, EpicsValue)> {
    let bit = |mask: u32, flag: u32| EpicsValue::Short(i16::from(mask & flag != 0));
    vec![
        ("TMSK".to_string(), EpicsValue::Long(trace_mask as i32)),
        ("TB0".to_string(), bit(trace_mask, TraceMask::ERROR.bits())),
        (
            "TB1".to_string(),
            bit(trace_mask, TraceMask::IO_DEVICE.bits()),
        ),
        (
            "TB2".to_string(),
            bit(trace_mask, TraceMask::IO_FILTER.bits()),
        ),
        (
            "TB3".to_string(),
            bit(trace_mask, TraceMask::IO_DRIVER.bits()),
        ),
        ("TB4".to_string(), bit(trace_mask, TraceMask::FLOW.bits())),
        (
            "TB5".to_string(),
            bit(trace_mask, TraceMask::WARNING.bits()),
        ),
        ("TIOM".to_string(), EpicsValue::Long(io_mask as i32)),
        ("TIB0".to_string(), bit(io_mask, TraceIoMask::ASCII.bits())),
        ("TIB1".to_string(), bit(io_mask, TraceIoMask::ESCAPE.bits())),
        ("TIB2".to_string(), bit(io_mask, TraceIoMask::HEX.bits())),
        ("TINM".to_string(), EpicsValue::Long(info_mask as i32)),
        (
            "TINB0".to_string(),
            bit(info_mask, TraceInfoMask::TIME.bits()),
        ),
        (
            "TINB1".to_string(),
            bit(info_mask, TraceInfoMask::PORT.bits()),
        ),
        (
            "TINB2".to_string(),
            bit(info_mask, TraceInfoMask::SOURCE.bits()),
        ),
        (
            "TINB3".to_string(),
            bit(info_mask, TraceInfoMask::THREAD.bits()),
        ),
    ]
}

// ===== Field descriptor table =====

static FIELD_LIST: &[FieldDesc] = &[
    // Address
    FieldDesc {
        name: "PORT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ADDR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "PCNCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DRVINFO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "REASON",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    // I/O control
    FieldDesc {
        name: "TMOD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TMOT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IFACE",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OCTETIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OPTIONIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "GPIBIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "I32IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "UI32IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "F64IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    // Octet output
    FieldDesc {
        name: "AOUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OEOS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "BOUT",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
    FieldDesc {
        name: "OMAX",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NOWT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NAWT",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OFMT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Octet input
    FieldDesc {
        name: "AINP",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "TINP",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "IEOS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "BINP",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "IMAX",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NRRD",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "IFMT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "EOMR",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    // Int32/UInt32/Float64
    FieldDesc {
        name: "I32INP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "I32OUT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "UI32INP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "UI32OUT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "UI32MASK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "F64INP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "F64OUT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Serial
    FieldDesc {
        name: "BAUD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LBAUD",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "PRTY",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DBIT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SBIT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MCTL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FCTL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXON",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXOFF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXANY",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // IP options
    FieldDesc {
        name: "HOSTINFO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DRTO",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // GPIB
    FieldDesc {
        name: "UCMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ACMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SPR",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    // Trace
    FieldDesc {
        name: "TMSK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB3",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB4",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB5",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIOM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TIB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TINB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB3",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TSIZ",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TFIL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // Connection management
    FieldDesc {
        name: "AUCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "CNCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ENBL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Misc
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ERRS",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "AQR",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
];

// ===== Trace bit helpers =====

impl AsynRecord {
    /// Rebuild TB0-TB5 from the trace mask value.
    fn update_trace_bits_from_mask(&mut self) {
        let mask = self.tmsk as u32;
        self.tb0 = if mask & TraceMask::ERROR.bits() != 0 {
            1
        } else {
            0
        };
        self.tb1 = if mask & TraceMask::IO_DEVICE.bits() != 0 {
            1
        } else {
            0
        };
        self.tb2 = if mask & TraceMask::IO_FILTER.bits() != 0 {
            1
        } else {
            0
        };
        self.tb3 = if mask & TraceMask::IO_DRIVER.bits() != 0 {
            1
        } else {
            0
        };
        self.tb4 = if mask & TraceMask::FLOW.bits() != 0 {
            1
        } else {
            0
        };
        self.tb5 = if mask & TraceMask::WARNING.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TMSK from TB0-TB5 bit fields.
    fn update_mask_from_trace_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tb0 != 0 {
            mask |= TraceMask::ERROR.bits();
        }
        if self.tb1 != 0 {
            mask |= TraceMask::IO_DEVICE.bits();
        }
        if self.tb2 != 0 {
            mask |= TraceMask::IO_FILTER.bits();
        }
        if self.tb3 != 0 {
            mask |= TraceMask::IO_DRIVER.bits();
        }
        if self.tb4 != 0 {
            mask |= TraceMask::FLOW.bits();
        }
        if self.tb5 != 0 {
            mask |= TraceMask::WARNING.bits();
        }
        self.tmsk = mask as i32;
    }

    /// Rebuild TIB0-TIB2 from trace I/O mask value.
    fn update_io_bits_from_mask(&mut self) {
        let mask = self.tiom as u32;
        self.tib0 = if mask & TraceIoMask::ASCII.bits() != 0 {
            1
        } else {
            0
        };
        self.tib1 = if mask & TraceIoMask::ESCAPE.bits() != 0 {
            1
        } else {
            0
        };
        self.tib2 = if mask & TraceIoMask::HEX.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TIOM from TIB0-TIB2.
    fn update_mask_from_io_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tib0 != 0 {
            mask |= TraceIoMask::ASCII.bits();
        }
        if self.tib1 != 0 {
            mask |= TraceIoMask::ESCAPE.bits();
        }
        if self.tib2 != 0 {
            mask |= TraceIoMask::HEX.bits();
        }
        self.tiom = mask as i32;
    }

    /// Rebuild TINB0-TINB3 from trace info mask value.
    fn update_info_bits_from_mask(&mut self) {
        let mask = self.tinm as u32;
        self.tinb0 = if mask & TraceInfoMask::TIME.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb1 = if mask & TraceInfoMask::PORT.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb2 = if mask & TraceInfoMask::SOURCE.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb3 = if mask & TraceInfoMask::THREAD.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TINM from TINB0-TINB3.
    fn update_mask_from_info_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tinb0 != 0 {
            mask |= TraceInfoMask::TIME.bits();
        }
        if self.tinb1 != 0 {
            mask |= TraceInfoMask::PORT.bits();
        }
        if self.tinb2 != 0 {
            mask |= TraceInfoMask::SOURCE.bits();
        }
        if self.tinb3 != 0 {
            mask |= TraceInfoMask::THREAD.bits();
        }
        self.tinm = mask as i32;
    }

    /// Resolve the trace target for this record's trace-control writes.
    ///
    /// C `findTracePvt` (asynManager.c:541-549) routes a trace mutation to
    /// the device `dpCommon` when the connected `pasynUser` names a device
    /// — a multi-device port addressed by `ADDR >= 0` — otherwise to the
    /// port-wide `dpc`. `Some(addr)` selects the device override;
    /// `None` the port default. Every `apply_trace_*` path routes through
    /// this single resolver so a record adjusting one address cannot mutate
    /// the whole port (or vice versa), and future trace controls inherit
    /// the rule by construction.
    fn trace_addr_target(&self) -> Option<i32> {
        match self.port_entry {
            Some(ref entry) if self.addr >= 0 && entry.handle.is_multi_device() => Some(self.addr),
            _ => None,
        }
    }

    /// Apply current trace mask fields to the TraceManager.
    fn apply_trace_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceMask::from_bits_truncate(self.tmsk as u32);
            match self.trace_addr_target() {
                Some(addr) => entry.trace.set_device_trace_mask(&self.port, addr, mask),
                None => entry.trace.set_trace_mask(Some(&self.port), mask),
            }
        }
    }

    /// Apply current trace I/O mask to the TraceManager.
    fn apply_trace_io_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceIoMask::from_bits_truncate(self.tiom as u32);
            match self.trace_addr_target() {
                Some(addr) => entry.trace.set_device_trace_io_mask(&self.port, addr, mask),
                None => entry.trace.set_trace_io_mask(Some(&self.port), mask),
            }
        }
    }

    /// Apply current trace info mask to the TraceManager.
    fn apply_trace_info_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceInfoMask::from_bits_truncate(self.tinm as u32);
            match self.trace_addr_target() {
                Some(addr) => entry
                    .trace
                    .set_device_trace_info_mask(&self.port, addr, mask),
                None => entry.trace.set_trace_info_mask(Some(&self.port), mask),
            }
        }
    }

    /// Apply truncate size to TraceManager.
    fn apply_trace_truncate_size(&self) {
        if let Some(ref entry) = self.port_entry {
            let size = self.tsiz as usize;
            match self.trace_addr_target() {
                Some(addr) => entry
                    .trace
                    .set_device_io_truncate_size(&self.port, addr, size),
                None => entry.trace.set_io_truncate_size(Some(&self.port), size),
            }
        }
    }

    /// Apply trace file to TraceManager.
    ///
    /// C parity: `special`/`asynRecordTFIL` (asynRecord.c:453-480) maps the
    /// `TFIL` string to a trace sink with the bracketed-token convention —
    /// empty and `<stdout>` -> stdout, `<stderr>` -> stderr, `<errlog>` ->
    /// the errlog sink, any other value a file path opened with
    /// `fopen(.., "a+")` so existing trace logs are appended, not
    /// truncated. On open failure C reports the error and leaves the
    /// current trace file unchanged (does not fall back to another sink).
    /// (The IOC-shell `asynSetTraceFile` uses a different convention —
    /// bare names, empty -> stderr, `fopen "w"` — handled in `iocsh.rs`.)
    fn apply_trace_file(&mut self) {
        let Some(entry) = self.port_entry.clone() else {
            return;
        };
        let tfil = self.tfil.clone();
        let file = match open_trace_file(&tfil) {
            Ok(file) => file,
            Err(e) => {
                self.errs = format!("Error opening trace file: {tfil}: {e}");
                return;
            }
        };
        match self.trace_addr_target() {
            Some(addr) => entry.trace.set_device_trace_file(&self.port, addr, file),
            None => entry.trace.set_trace_file(Some(&self.port), file),
        }
    }

    /// Read current trace state from TraceManager into record fields.
    ///
    /// C `monitorStatus` (asynRecord.c:1066-1084) refreshes the trace
    /// mask, the trace I/O mask, AND the trace info mask (`TINM`/`TINB0..3`)
    /// from the trace manager. Previously the info mask was never imported,
    /// so a record connecting after a non-default `asynSetTraceInfoMask`
    /// showed `TINM`/`TINB*` as zero.
    fn read_trace_state(&mut self) {
        let (trace_mask, io_mask, info_mask) = match self.port_entry {
            Some(ref entry) => {
                let port = &self.port;
                (
                    entry.trace.get_trace_mask(Some(port)).bits(),
                    entry.trace.get_trace_io_mask(Some(port)).bits(),
                    entry.trace.get_trace_info_mask(Some(port)).bits(),
                )
            }
            None => return,
        };

        self.tmsk = trace_mask as i32;
        self.update_trace_bits_from_mask();

        self.tiom = io_mask as i32;
        self.update_io_bits_from_mask();

        self.tinm = info_mask as i32;
        self.update_info_bits_from_mask();
    }

    /// Subscribe to the port's trace exceptions so a later external
    /// `asynSetTrace{Mask,IOMask,InfoMask}` is reflected in the record's
    /// readback fields.
    ///
    /// C registers `exceptCallback` with `exceptionCallbackAdd` in
    /// `connectDevice` (asynRecord.c:1269); the callback re-runs
    /// `monitorStatus` (asynRecord.c:903-917,1066-1117) under `dbScanLock`,
    /// which re-imports the trace masks and posts the changed fields
    /// immediately, out of band — it does NOT wait for the next `process()`.
    ///
    /// The Rust analogue posts through the merged PACT seam: when the record
    /// carries a database handle (`async_ctx`) and a runtime is available, the
    /// callback recomputes the trace readback fields from the trace manager
    /// and `post_fields`-es them now (the C `db_post_events` under
    /// `dbScanLock`). Like C `POST_IF_NEW` (asynRecord.c:210-214) it keeps a
    /// last-posted cache and posts only the fields whose value changed, so an
    /// unrelated `setTrace*` does not re-post unchanged readback fields. With
    /// no handle / runtime — e.g. a record connected outside a database — it
    /// falls back to raising a dirty flag that `process()` drains through the
    /// single `read_trace_state` owner.
    fn register_trace_exception_callback(&mut self) {
        self.clear_trace_exception_callback();
        let Some(ref entry) = self.port_entry else {
            return;
        };
        let Some(mgr) = entry.trace.exception_manager() else {
            return;
        };
        let port = self.port.clone();
        let trace = entry.trace.clone();
        let dirty = Arc::clone(&self.trace_status_dirty);
        // The immediate out-of-band post needs both a database handle (to post
        // through) and a runtime handle (the exception fires from a
        // `setTrace*` caller's thread — iocsh or the port actor — which is not
        // a tokio worker, so `tokio::spawn` would panic; an explicit `Handle`
        // submits to the runtime from any thread). Capture them once here,
        // where registration runs in the database's async init context.
        let immediate = match (
            self.async_ctx.clone(),
            tokio::runtime::Handle::try_current().ok(),
        ) {
            (Some((name, db)), Some(handle)) => Some((name, db, handle)),
            _ => None,
        };
        // C `monitorStatus` posts a trace readback field only when it differs
        // from the per-record remembered value (`POST_IF_NEW`,
        // asynRecord.c:210-214,1102-1117). The base `post_fields` path posts
        // unconditionally, so the out-of-band re-post keeps its own last-posted
        // cache — the asynRecord `old` analogue — to avoid re-posting unchanged
        // trace fields on every `setTrace*`. Seed it with the current values
        // (the C `old` after the connect-path `monitorStatus`) so the first
        // external change posts only the fields that actually changed.
        let last_posted: Arc<Mutex<HashMap<String, EpicsValue>>> = Arc::new(Mutex::new(
            trace_readback_fields(
                trace.get_trace_mask(Some(&port)).bits(),
                trace.get_trace_io_mask(Some(&port)).bits(),
                trace.get_trace_info_mask(Some(&port)).bits(),
            )
            .into_iter()
            .collect(),
        ));
        let id = mgr.add_callback(move |ev| {
            // Only the trace masks `monitorStatus` recomputes matter here; the
            // announce for a port-level `setTrace*` carries the port name
            // (TraceManager::announce uses addr -1).
            if ev.port_name != port
                || !matches!(
                    ev.exception,
                    AsynException::TraceMask
                        | AsynException::TraceIoMask
                        | AsynException::TraceInfoMask
                )
            {
                return;
            }
            match &immediate {
                Some((name, db, handle)) => {
                    // Recompute the current trace readback fields and post them
                    // out of band now — the C `exceptCallback` → `monitorStatus`
                    // immediate re-post (asynRecord.c:1102-1117). Mirror C
                    // `POST_IF_NEW` (asynRecord.c:210-214): post only the fields
                    // whose value changed since the last out-of-band post and
                    // remember the new values, so an unchanged trace field is
                    // not re-posted.
                    let changed: Vec<(String, EpicsValue)> = {
                        let mut cache = last_posted.lock().unwrap();
                        let mut changed = Vec::new();
                        for (field, value) in trace_readback_fields(
                            trace.get_trace_mask(Some(&port)).bits(),
                            trace.get_trace_io_mask(Some(&port)).bits(),
                            trace.get_trace_info_mask(Some(&port)).bits(),
                        ) {
                            if cache.get(&field) != Some(&value) {
                                cache.insert(field.clone(), value.clone());
                                changed.push((field, value));
                            }
                        }
                        changed
                    };
                    if changed.is_empty() {
                        return;
                    }
                    let (name, db) = (name.clone(), db.clone());
                    handle.spawn(async move {
                        let _ = db.post_fields(&name, changed).await;
                    });
                }
                None => {
                    dirty.store(true, Ordering::Release);
                }
            }
        });
        self.trace_except_cb = Some((mgr, id));
    }

    /// Remove the trace exception subscription (C `exceptionCallbackRemove`,
    /// asynRecord.c:523,1154,1313). Idempotent.
    fn clear_trace_exception_callback(&mut self) {
        if let Some((mgr, id)) = self.trace_except_cb.take() {
            mgr.remove_callback(id);
        }
    }

    /// Read serial/IP options from the driver into record fields.
    fn read_options_from_driver(&mut self, handle: &PortHandle) {
        // Baud rate
        if let Ok(val) = handle.get_option_blocking("baud") {
            self.lbaud = val.parse::<i32>().unwrap_or(0);
            self.baud = baud_rate_to_menu_index(self.lbaud);
        }
        // Parity
        if let Ok(val) = handle.get_option_blocking("parity") {
            self.prty = match val.as_str() {
                "none" => 1,
                "even" => 2,
                "odd" => 3,
                _ => 0, // unknown
            };
        }
        // Data bits. The serial driver's get_option exposes data bits
        // under the key `"bits"` (C asynRecord.c:1884 likewise reads
        // "bits"); `"csize"` is consumed by no driver, so the readback
        // must use "bits" to match the DBIT write path (write_option
        // "bits" below).
        if let Ok(val) = handle.get_option_blocking("bits") {
            self.dbit = match val.as_str() {
                "5" => 1,
                "6" => 2,
                "7" => 3,
                "8" => 4,
                _ => 0,
            };
        }
        // Stop bits
        if let Ok(val) = handle.get_option_blocking("stop") {
            self.sbit = match val.as_str() {
                "1" => 1,
                "2" => 2,
                _ => 0,
            };
        }
        // Flow control
        if let Ok(val) = handle.get_option_blocking("crtscts") {
            self.fctl = match val.as_str() {
                "Y" | "Yes" => 2,         // Hardware
                "N" | "No" | "none" => 1, // None
                _ => 0,
            };
        }
        // Modem control
        if let Ok(val) = handle.get_option_blocking("clocal") {
            self.mctl = match val.as_str() {
                "Y" | "Yes" => 1, // CLOCAL
                "N" | "No" => 2,  // YES (hardware modem control)
                _ => 0,
            };
        }
        // XON/XOFF
        if let Ok(val) = handle.get_option_blocking("ixon") {
            self.ixon = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        if let Ok(val) = handle.get_option_blocking("ixoff") {
            self.ixoff = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        if let Ok(val) = handle.get_option_blocking("ixany") {
            self.ixany = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        // IP options
        if let Ok(val) = handle.get_option_blocking("hostinfo") {
            self.hostinfo = val;
        }
        if let Ok(val) = handle.get_option_blocking("disconnectOnReadTimeout") {
            self.drto = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
    }

    /// Write a serial/IP option to the driver via SetOption.
    fn write_option(&mut self, key: &str, value: &str) {
        if let Some(ref entry) = self.port_entry {
            if let Err(e) = entry.handle.set_option_blocking(key, value) {
                self.errs = format!("set_option({key}): {e}");
            }
        }
    }

    /// Refresh CNCT from the port's *transport* state — the single owner of
    /// that field's value.
    ///
    /// C parity: CNCT is a readback, not a latch. `monitorStatus`
    /// (asynRecord.c:1089-1093) assigns it from `pasynManager->isConnected` on
    /// every cycle, and `isConnected` fails (→ CNCT=0) when the record is bound
    /// to no port. It says nothing about whether *this record* found its port —
    /// that is PCNCT (asynRecord.c:519-527, `connectDevice` /
    /// `pasynManager->disconnect`). Driving CNCT anywhere else is what gave the
    /// field two meanings: `connect_device` used to latch `cnct = 1` on a
    /// successful *attach*, so a registered-but-unconnected port (noAutoConnect,
    /// or a link that dropped) reported "Connected" on a dead wire.
    fn refresh_connected_state(&mut self) {
        let connected = match self.port_entry {
            Some(ref entry) => entry.handle.is_connected_blocking().unwrap_or(false),
            None => false,
        };
        self.cnct = i32::from(connected);
    }

    /// Attempt to connect to the port specified in the PORT field.
    fn connect_device(&mut self) {
        if self.port.is_empty() {
            self.pcnct = 0;
            self.port_entry = None;
            self.refresh_connected_state();
            self.clear_trace_exception_callback();
            return;
        }

        match registry::get_port(&self.port) {
            Some(entry) => {
                // Resolve drvinfo → reason if specified
                if !self.drvinfo.is_empty() {
                    match entry
                        .handle
                        .drv_user_create_blocking(&self.drvinfo, self.addr)
                    {
                        Ok(info) => {
                            self.resolved_reason = info.reason;
                            self.reason = info.reason as i32;
                        }
                        Err(e) => {
                            self.errs = format!("drvUserCreate failed: {e}");
                            self.resolved_reason = 0;
                        }
                    }
                } else {
                    self.resolved_reason = self.reason as usize;
                }

                // All standard interfaces valid for our ports
                self.octetiv = 1;
                self.i32iv = 1;
                self.ui32iv = 1;
                self.f64iv = 1;
                self.optioniv = 1;
                self.gpibiv = 0; // No GPIB hardware in Rust ports

                // Read trace state from manager
                self.port_entry = Some(entry.clone());
                self.read_trace_state();
                // C `connectDevice` adds `exceptCallback` here
                // (asynRecord.c:1269) so later external trace changes
                // refresh the readback fields.
                self.register_trace_exception_callback();

                // Read serial/IP options from driver
                self.read_options_from_driver(&entry.handle);

                // Attached to the port (C `connectDevice` sets PCNCT=1,
                // asynRecord.c:1305).
                self.pcnct = 1;
                // C asynRecord.c connectDevice queries the port's actual
                // enable / auto-connect / connect state into ENBL / AUCT /
                // CNCT — it must not force them to 1, which would discard a
                // user who configured ENBL=0 or a port registered
                // noAutoConnect, and would claim a live wire on a port whose
                // transport is down. Keep the current field value if the query
                // fails.
                if let Ok(enabled) = entry.handle.is_enabled_blocking() {
                    self.enbl = i32::from(enabled);
                }
                if let Ok(auto) = entry.handle.is_auto_connect_blocking() {
                    self.auct = i32::from(auto);
                }
                self.refresh_connected_state();
                // Only clear errors if drv_user_create succeeded (don't mask the error)
                if self.errs.is_empty() || self.resolved_reason != 0 || self.drvinfo.is_empty() {
                    self.errs.clear();
                }
            }
            None => {
                self.errs = format!("port '{}' not found", self.port);
                self.pcnct = 0;
                self.port_entry = None;
                self.refresh_connected_state();
                self.clear_trace_exception_callback();
            }
        }
    }

    /// Build the octet output payload by `OFMT`, mirroring
    /// `asynRecord.c:1486-1502`:
    ///   - ASCII  -> `dbTranslateEscape(AOUT)`: the AOUT string with escape
    ///     sequences (`\r\n` -> CRLF) translated.
    ///   - Hybrid -> `dbTranslateEscape(BOUT read as a C string)`: the binary
    ///     output buffer, escape-translated (stops at the first NUL).
    ///   - Binary -> raw BOUT, `NOWT` bytes clamped to `OMAX`, no translation.
    ///
    /// ASCII/Hybrid emit the full translated buffer; only Binary is
    /// length-bounded by `NOWT`/`OMAX`. Previously the record sent raw AOUT
    /// for both ASCII and Hybrid (ASCII shipped literal backslashes, Hybrid
    /// ignored BOUT) and clamped every mode by `NOWT`.
    fn octet_output_buffer(&self) -> Vec<u8> {
        match self.ofmt {
            ASYN_FMT_BINARY => {
                let omax = self.omax.max(0) as usize;
                let nowt = (self.nowt.max(0) as usize).min(omax);
                self.bout[..nowt.min(self.bout.len())].to_vec()
            }
            ASYN_FMT_HYBRID => {
                let end = self
                    .bout
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(self.bout.len());
                translate_escape_bytes(&self.bout[..end])
            }
            _ => translate_escape(&self.aout),
        }
    }

    /// Perform I/O based on TMOD and IFACE.
    /// Snapshot the record fields `performIO` reads into an [`IoPlan`] so the
    /// I/O can run without touching the record (synchronously here, or off
    /// the scan thread in [`run_io_plan`]).
    fn build_io_plan(&self) -> IoPlan {
        let octet_out = self.octet_output_buffer();
        let octet_out_len = octet_out.len();
        // C `performOctetIO` (asynRecord.c:1503-1517): the input buffer is
        // chosen by IFMT — ASCII reads into the fixed 40-byte AINP string
        // (`inlen = sizeof(ainp)`), Hybrid and Binary into the IMAX-sized BINP
        // buffer — and the read length is NRRD clamped to that capacity, or the
        // whole capacity when NRRD is 0. IMAX must NOT size an ASCII read: with
        // the default IMAX=80 that would let a terminator-less response consume
        // 80 bytes with no overflow alarm, where C stops at 40, raises
        // READ/MINOR and leaves the rest in the driver.
        //
        // Clamp against negative IMAX/NRRD — both are settable Long fields; a
        // negative value sign-extends to a huge usize and would request a
        // multi-GB buffer.
        let in_len = if self.ifmt == ASYN_FMT_ASCII {
            AINP_SIZE
        } else {
            self.imax.max(0) as usize
        };
        let octet_buf_size = if self.nrrd > 0 {
            (self.nrrd as usize).min(in_len)
        } else {
            in_len
        };
        // C `asynRecord.c` sets `pasynUser->timeout = precord->tmot` before
        // every transfer; a non-positive `tmot` falls back to the 1 s default.
        let timeout = if self.tmot > 0.0 {
            std::time::Duration::from_secs_f64(self.tmot)
        } else {
            std::time::Duration::from_secs(1)
        };
        IoPlan {
            tmod: TransferMode::from_u16(self.tmod as u16),
            iface: InterfaceType::from_u16(self.iface as u16),
            reason: self.resolved_reason,
            addr: self.addr,
            timeout,
            octet_out,
            octet_out_len,
            ofmt: self.ofmt,
            i32out: self.i32out,
            ui32out: self.ui32out,
            ui32mask: self.ui32mask,
            f64out: self.f64out,
            octet_buf_size,
            in_len,
            ifmt: self.ifmt,
        }
    }

    /// Apply the results of a `performIO` cycle to the record's input/status
    /// fields. Single owner of the result→field mapping (C `performIO`
    /// stores into AINP/BINP/I32INP/.../NAWT/EOMR/NORD/ERRS); both the
    /// synchronous [`Self::perform_io`] and the off-thread [`run_io_plan`]
    /// feed it, so the mapping cannot drift between the two paths.
    fn apply_io_outcome(&mut self, out: IoOutcome) {
        if let Some(v) = out.nawt {
            self.nawt = v;
        }
        if let Some(v) = out.eomr {
            self.eomr = v;
        }
        if let Some(v) = out.nord {
            self.nord = v;
        }
        if let Some(v) = out.tinp {
            self.tinp = v;
        }
        if let Some(v) = out.ainp {
            self.ainp = v;
        }
        if let Some(v) = out.binp {
            self.binp = v;
        }
        if let Some(v) = out.i32inp {
            self.i32inp = v;
        }
        if let Some(v) = out.ui32inp {
            self.ui32inp = v;
        }
        if let Some(v) = out.f64inp {
            self.f64inp = v;
        }
        if let Some(v) = out.errs {
            self.errs = v;
        }
        // The I/O cycle's record alarm (C `recGblSetSevr` in `performIO`).
        // `check_alarms` — invoked by the framework on this same completion
        // cycle (sync `process()` return or async re-entry) — is the sole
        // consumer; it `take()`s and commits it via `rec_gbl_set_sevr`.
        if let Some(a) = out.alarm {
            self.io_alarm = Some(a);
        }
    }

    /// Synchronous `performIO` — the C `process()` `canBlock==0` branch
    /// (asynRecord.c:351-352) that runs the I/O inline rather than queuing
    /// it. Shares the op builders and result recorders with the off-thread
    /// [`run_io_plan`]; only the submit primitive differs (blocking here,
    /// awaited there).
    fn perform_io(&mut self) -> CaResult<()> {
        let entry = match &self.port_entry {
            Some(e) => e.clone(),
            None => {
                self.errs = "not connected".to_string();
                return Ok(());
            }
        };
        let plan = self.build_io_plan();
        let mut out = IoOutcome::default();

        for phase in io_phases(&plan) {
            let res = match phase {
                IoPhase::Flush => entry
                    .handle
                    .submit_blocking(RequestOp::Flush, flush_user(&plan)),
                IoPhase::Write => entry
                    .handle
                    .submit_blocking(io_write_op(&plan), io_user(&plan)),
                IoPhase::Read => entry
                    .handle
                    .submit_blocking(io_read_op(&plan), io_user(&plan)),
            };
            record_phase_result(&plan, &mut out, phase, res);
        }

        self.apply_io_outcome(out);
        Ok(())
    }

    /// Queue `performIO` to run off the scan thread (C `process()`
    /// `canBlock!=0` branch, asynRecord.c:344-350). Submits the I/O to the
    /// port actor non-blocking, then wires the actor completion to a fresh
    /// async-record token so `process()` re-enters and applies the result —
    /// the Rust analogue of `asynCallbackProcess` →
    /// `callbackRequestProcessCallback` (asynRecord.c:808-831). Holds the
    /// `state = stateIO` request in `io_inflight` and returns `AsyncPending`.
    fn spawn_async_io(
        &mut self,
        handle: PortHandle,
        name: String,
        db: AsyncDbHandle,
    ) -> ProcessOutcome {
        let plan = self.build_io_plan();
        let cancel = CancelToken::new();
        let slot: Arc<Mutex<Option<IoOutcome>>> = Arc::new(Mutex::new(None));

        let cancel_task = cancel.clone();
        let slot_task = slot.clone();
        tokio::spawn(async move {
            let outcome = run_io_plan(handle, plan, cancel_task).await;
            *slot_task.lock().unwrap() = Some(outcome);
            // Mint a fresh re-entry token (superseding any older one) wired
            // to an already-fired completion, so the waiting record re-enters
            // process() now and applies the result — same shape as sseq's
            // force_finish_reentry / WAITn completion. A token whose record
            // was meanwhile removed (mint `None`) or superseded by an AQR
            // cancel re-enters nothing, by the generation gate.
            if let Some(token) = db.mint_async_token(&name).await {
                let (waitset, completion) = AsyncDbHandle::new_put_notify();
                waitset.leave();
                let _ = db.reprocess_on_notify(token, completion);
            }
        });

        self.io_inflight = Some(IoInFlight {
            cancel,
            result: slot,
        });
        ProcessOutcome::async_pending()
    }
}

// ===== asynRecord DBF_MENU choice tables =====
//
// Verbatim from `asynRecord.dbd` `menu(...)` definitions, in menu index
// order (index = stored value). Served to clients as the `DBR_ENUM`
// choice strings via `menu_field_choices` below: the framework promotes a
// matching `Short` field value to `DBR_ENUM` and attaches these labels
// (`RecordInstance::promote_menu_value` / `attach_menu_enum`). Without
// them every `DBF_MENU` field is served as a plain numeric `Short`, so
// MEDM `choice button` / `menu` widgets bound to TB0..TB5 / IFACE / CNCT /
// BAUD / ... render blank (no states to label), while `text entry`
// widgets on the same screen show their numbers fine.
const MENU_ASYN_TMOD: &[&str] = &["Write/Read", "Write", "Read", "Flush", "NoI/O"];
const MENU_ASYN_INTERFACE: &[&str] =
    &["asynOctet", "asynInt32", "asynUInt32Digital", "asynFloat64"];
const MENU_ASYN_FMT: &[&str] = &["ASCII", "Hybrid", "Binary"];
const MENU_ASYN_TRACE: &[&str] = &["Off", "On"];
const MENU_ASYN_AUTOCONNECT: &[&str] = &["noAutoConnect", "autoConnect"];
const MENU_ASYN_CONNECT: &[&str] = &["Disconnect", "Connect"];
const MENU_ASYN_ENABLE: &[&str] = &["Disable", "Enable"];
const MENU_ASYN_EOMREASON: &[&str] = &[
    "None",
    "Count",
    "Eos",
    "Count Eos",
    "End",
    "Count End",
    "Eos End",
    "Count Eos End",
];
const MENU_SERIAL_BAUD: &[&str] = &[
    "Unknown", "300", "600", "1200", "2400", "4800", "9600", "19200", "38400", "57600", "115200",
    "230400", "460800", "576000", "921600", "1152000",
];
const MENU_SERIAL_PRTY: &[&str] = &["Unknown", "None", "Even", "Odd"];
const MENU_SERIAL_DBIT: &[&str] = &["Unknown", "5", "6", "7", "8"];
const MENU_SERIAL_SBIT: &[&str] = &["Unknown", "1", "2"];
const MENU_SERIAL_MCTL: &[&str] = &["Unknown", "CLOCAL", "YES"];
const MENU_SERIAL_FCTL: &[&str] = &["Unknown", "None", "Hardware"];
const MENU_SERIAL_IX: &[&str] = &["Unknown", "No", "Yes"];
const MENU_IP_DRTO: &[&str] = &["Unknown", "No", "Yes"];
const MENU_GPIB_UCMD: &[&str] = &[
    "None",
    "Device Clear (DCL)",
    "Local Lockout (LL0)",
    "Serial Poll Disable (SPD)",
    "Serial Poll Enable (SPE)",
    "Unlisten (UNL)",
    "Untalk (UNT)",
];
const MENU_GPIB_ACMD: &[&str] = &[
    "None",
    "Group Execute Trig. (GET)",
    "Go To Local (GTL)",
    "Selected Dev. Clear (SDC)",
    "Take Control (TCT)",
    "Serial Poll",
];

// ===== Record trait implementation =====

impl Record for AsynRecord {
    fn record_type(&self) -> &'static str {
        "asyn"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELD_LIST
    }

    /// Choice strings for every asynRecord `DBF_MENU` field, in menu index
    /// order (verbatim from `asynRecord.dbd`). The field value is held as a
    /// `Short` menu index; returning the choices here makes the framework
    /// promote it to `DBR_ENUM` and serve these labels, so MEDM
    /// `choice button` / `menu` widgets render. C `dbStaticLib` serves any
    /// `DBF_MENU` field as `DBR_ENUM` with its `menu()` choices — this
    /// restores that parity for the asyn record.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "TMOD" => Some(MENU_ASYN_TMOD),
            "IFACE" => Some(MENU_ASYN_INTERFACE),
            "OFMT" | "IFMT" => Some(MENU_ASYN_FMT),
            "TB0" | "TB1" | "TB2" | "TB3" | "TB4" | "TB5" | "TIB0" | "TIB1" | "TIB2" | "TINB0"
            | "TINB1" | "TINB2" | "TINB3" => Some(MENU_ASYN_TRACE),
            "AUCT" => Some(MENU_ASYN_AUTOCONNECT),
            "CNCT" | "PCNCT" => Some(MENU_ASYN_CONNECT),
            "ENBL" => Some(MENU_ASYN_ENABLE),
            "EOMR" => Some(MENU_ASYN_EOMREASON),
            "BAUD" => Some(MENU_SERIAL_BAUD),
            "PRTY" => Some(MENU_SERIAL_PRTY),
            "DBIT" => Some(MENU_SERIAL_DBIT),
            "SBIT" => Some(MENU_SERIAL_SBIT),
            "MCTL" => Some(MENU_SERIAL_MCTL),
            "FCTL" => Some(MENU_SERIAL_FCTL),
            "IXON" | "IXOFF" | "IXANY" => Some(MENU_SERIAL_IX),
            "DRTO" => Some(MENU_IP_DRTO),
            "UCMD" => Some(MENU_GPIB_UCMD),
            "ACMD" => Some(MENU_GPIB_ACMD),
            _ => None,
        }
    }

    /// Stash the canonical record name + a cycle-free database handle the
    /// framework supplies at `add_record`. Enables the non-blocking
    /// `process()` completion re-entry and the out-of-band trace post; a
    /// record never built into a database keeps `None` and stays fully
    /// synchronous.
    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
    }

    /// C `performIO`'s `recGblSetSevr` calls raise a record alarm severity for
    /// every I/O failure: read error -> READ/MAJOR, ASCII/Hybrid input overflow
    /// -> READ/MINOR, register write error -> WRITE/MAJOR, missing GPIB
    /// interface -> COMM/MAJOR (asynRecord.c:1380-1621/1649/1695). The record
    /// stages the cycle's alarm in `io_alarm` as the I/O result is applied; the
    /// framework calls this hook on the same completion cycle (sync return or
    /// async re-entry), where it commits the staged alarm via `rec_gbl_set_sevr`
    /// — the single owner of the I/O-error → NSEV/NSTA transition. `take()`
    /// consumes it so it raises exactly once.
    fn check_alarms(&mut self, common: &mut CommonFields) {
        if let Some((stat, sevr)) = self.io_alarm.take() {
            rec_gbl_set_sevr(common, stat, sevr);
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "PORT" => Some(EpicsValue::String(self.port.clone().into())),
            "ADDR" => Some(EpicsValue::Long(self.addr)),
            "PCNCT" => Some(EpicsValue::Short(self.pcnct as i16)),
            "DRVINFO" => Some(EpicsValue::String(self.drvinfo.clone().into())),
            "REASON" => Some(EpicsValue::Long(self.reason)),
            "TMOD" => Some(EpicsValue::Short(self.tmod as i16)),
            "TMOT" => Some(EpicsValue::Double(self.tmot)),
            "IFACE" => Some(EpicsValue::Short(self.iface as i16)),
            "OCTETIV" => Some(EpicsValue::Long(self.octetiv)),
            "OPTIONIV" => Some(EpicsValue::Long(self.optioniv)),
            "GPIBIV" => Some(EpicsValue::Long(self.gpibiv)),
            "I32IV" => Some(EpicsValue::Long(self.i32iv)),
            "UI32IV" => Some(EpicsValue::Long(self.ui32iv)),
            "F64IV" => Some(EpicsValue::Long(self.f64iv)),
            "AOUT" => Some(EpicsValue::String(self.aout.clone().into())),
            "OEOS" => Some(EpicsValue::String(self.oeos.clone().into())),
            "BOUT" => Some(EpicsValue::CharArray(self.bout.clone())),
            "OMAX" => Some(EpicsValue::Long(self.omax)),
            "NOWT" => Some(EpicsValue::Long(self.nowt)),
            "NAWT" => Some(EpicsValue::Long(self.nawt)),
            "OFMT" => Some(EpicsValue::Short(self.ofmt as i16)),
            "AINP" => Some(EpicsValue::String(self.ainp.clone().into())),
            "TINP" => Some(EpicsValue::String(self.tinp.clone().into())),
            "IEOS" => Some(EpicsValue::String(self.ieos.clone().into())),
            "BINP" => Some(EpicsValue::CharArray(self.binp.clone())),
            "IMAX" => Some(EpicsValue::Long(self.imax)),
            "NRRD" => Some(EpicsValue::Long(self.nrrd)),
            "NORD" => Some(EpicsValue::Long(self.nord)),
            "IFMT" => Some(EpicsValue::Short(self.ifmt as i16)),
            "EOMR" => Some(EpicsValue::Short(self.eomr as i16)),
            "I32INP" => Some(EpicsValue::Long(self.i32inp)),
            "I32OUT" => Some(EpicsValue::Long(self.i32out)),
            "UI32INP" => Some(EpicsValue::Long(self.ui32inp as i32)),
            "UI32OUT" => Some(EpicsValue::Long(self.ui32out as i32)),
            "UI32MASK" => Some(EpicsValue::Long(self.ui32mask as i32)),
            "F64INP" => Some(EpicsValue::Double(self.f64inp)),
            "F64OUT" => Some(EpicsValue::Double(self.f64out)),
            "BAUD" => Some(EpicsValue::Short(self.baud as i16)),
            "LBAUD" => Some(EpicsValue::Long(self.lbaud)),
            "PRTY" => Some(EpicsValue::Short(self.prty as i16)),
            "DBIT" => Some(EpicsValue::Short(self.dbit as i16)),
            "SBIT" => Some(EpicsValue::Short(self.sbit as i16)),
            "MCTL" => Some(EpicsValue::Short(self.mctl as i16)),
            "FCTL" => Some(EpicsValue::Short(self.fctl as i16)),
            "IXON" => Some(EpicsValue::Short(self.ixon as i16)),
            "IXOFF" => Some(EpicsValue::Short(self.ixoff as i16)),
            "IXANY" => Some(EpicsValue::Short(self.ixany as i16)),
            "HOSTINFO" => Some(EpicsValue::String(self.hostinfo.clone().into())),
            "DRTO" => Some(EpicsValue::Short(self.drto as i16)),
            "UCMD" => Some(EpicsValue::Short(self.ucmd as i16)),
            "ACMD" => Some(EpicsValue::Short(self.acmd as i16)),
            "SPR" => Some(EpicsValue::Char(self.spr as u8)),
            "TMSK" => Some(EpicsValue::Long(self.tmsk)),
            "TB0" => Some(EpicsValue::Short(self.tb0 as i16)),
            "TB1" => Some(EpicsValue::Short(self.tb1 as i16)),
            "TB2" => Some(EpicsValue::Short(self.tb2 as i16)),
            "TB3" => Some(EpicsValue::Short(self.tb3 as i16)),
            "TB4" => Some(EpicsValue::Short(self.tb4 as i16)),
            "TB5" => Some(EpicsValue::Short(self.tb5 as i16)),
            "TIOM" => Some(EpicsValue::Long(self.tiom)),
            "TIB0" => Some(EpicsValue::Short(self.tib0 as i16)),
            "TIB1" => Some(EpicsValue::Short(self.tib1 as i16)),
            "TIB2" => Some(EpicsValue::Short(self.tib2 as i16)),
            "TINM" => Some(EpicsValue::Long(self.tinm)),
            "TINB0" => Some(EpicsValue::Short(self.tinb0 as i16)),
            "TINB1" => Some(EpicsValue::Short(self.tinb1 as i16)),
            "TINB2" => Some(EpicsValue::Short(self.tinb2 as i16)),
            "TINB3" => Some(EpicsValue::Short(self.tinb3 as i16)),
            "TSIZ" => Some(EpicsValue::Long(self.tsiz)),
            "TFIL" => Some(EpicsValue::String(self.tfil.clone().into())),
            "AUCT" => Some(EpicsValue::Short(self.auct as i16)),
            "CNCT" => Some(EpicsValue::Short(self.cnct as i16)),
            "ENBL" => Some(EpicsValue::Short(self.enbl as i16)),
            "VAL" => Some(EpicsValue::Long(self.val)),
            "ERRS" => Some(EpicsValue::String(self.errs.clone().into())),
            "AQR" => Some(EpicsValue::Char(self.aqr as u8)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Helper closures for type coercion
        let to_i32 = |v: &EpicsValue| -> i32 { v.to_f64().unwrap_or(0.0) as i32 };
        let to_u32 = |v: &EpicsValue| -> u32 { v.to_f64().unwrap_or(0.0) as u32 };
        let to_f64 = |v: &EpicsValue| -> f64 { v.to_f64().unwrap_or(0.0) };
        let to_str = |v: &EpicsValue| -> String { format!("{v}") };
        let to_bytes = |v: &EpicsValue| -> Vec<u8> {
            match v {
                EpicsValue::CharArray(b) => b.clone(),
                EpicsValue::String(s) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            }
        };

        match name {
            "PORT" => {
                self.port = to_str(&value);
            }
            "ADDR" => {
                self.addr = to_i32(&value);
            }
            "PCNCT" => {
                self.pcnct = to_i32(&value);
            }
            "DRVINFO" => {
                self.drvinfo = to_str(&value);
            }
            "REASON" => {
                self.reason = to_i32(&value);
            }
            "TMOD" => {
                self.tmod = to_i32(&value);
            }
            "TMOT" => {
                self.tmot = to_f64(&value);
            }
            "IFACE" => {
                self.iface = to_i32(&value);
            }
            "OCTETIV" => {
                self.octetiv = to_i32(&value);
            }
            "OPTIONIV" => {
                self.optioniv = to_i32(&value);
            }
            "GPIBIV" => {
                self.gpibiv = to_i32(&value);
            }
            "I32IV" => {
                self.i32iv = to_i32(&value);
            }
            "UI32IV" => {
                self.ui32iv = to_i32(&value);
            }
            "F64IV" => {
                self.f64iv = to_i32(&value);
            }
            "AOUT" => {
                self.aout = to_str(&value);
            }
            "OEOS" => {
                self.oeos = to_str(&value);
            }
            "BOUT" => {
                self.bout = to_bytes(&value);
            }
            "OMAX" => {
                self.omax = to_i32(&value);
            }
            "NOWT" => {
                self.nowt = to_i32(&value);
            }
            "NAWT" => {
                self.nawt = to_i32(&value);
            }
            "OFMT" => {
                self.ofmt = to_i32(&value);
            }
            "AINP" => {
                self.ainp = to_str(&value);
            }
            "TINP" => {
                self.tinp = to_str(&value);
            }
            "IEOS" => {
                self.ieos = to_str(&value);
            }
            "BINP" => {
                self.binp = to_bytes(&value);
            }
            "IMAX" => {
                self.imax = to_i32(&value);
            }
            "NRRD" => {
                self.nrrd = to_i32(&value);
            }
            "NORD" => {
                self.nord = to_i32(&value);
            }
            "IFMT" => {
                self.ifmt = to_i32(&value);
            }
            "EOMR" => {
                self.eomr = to_i32(&value);
            }
            "I32INP" => {
                self.i32inp = to_i32(&value);
            }
            "I32OUT" => {
                self.i32out = to_i32(&value);
            }
            "UI32INP" => {
                self.ui32inp = to_u32(&value);
            }
            "UI32OUT" => {
                self.ui32out = to_u32(&value);
            }
            "UI32MASK" => {
                self.ui32mask = to_u32(&value);
            }
            "F64INP" => {
                self.f64inp = to_f64(&value);
            }
            "F64OUT" => {
                self.f64out = to_f64(&value);
            }
            "BAUD" => {
                self.baud = to_i32(&value);
            }
            "LBAUD" => {
                self.lbaud = to_i32(&value);
            }
            "PRTY" => {
                self.prty = to_i32(&value);
            }
            "DBIT" => {
                self.dbit = to_i32(&value);
            }
            "SBIT" => {
                self.sbit = to_i32(&value);
            }
            "MCTL" => {
                self.mctl = to_i32(&value);
            }
            "FCTL" => {
                self.fctl = to_i32(&value);
            }
            "IXON" => {
                self.ixon = to_i32(&value);
            }
            "IXOFF" => {
                self.ixoff = to_i32(&value);
            }
            "IXANY" => {
                self.ixany = to_i32(&value);
            }
            "HOSTINFO" => {
                self.hostinfo = to_str(&value);
            }
            "DRTO" => {
                self.drto = to_i32(&value);
            }
            "UCMD" => {
                self.ucmd = to_i32(&value);
            }
            "ACMD" => {
                self.acmd = to_i32(&value);
            }
            "SPR" => {
                self.spr = to_i32(&value);
            }
            "TMSK" => {
                self.tmsk = to_i32(&value);
            }
            "TB0" => {
                self.tb0 = to_i32(&value);
            }
            "TB1" => {
                self.tb1 = to_i32(&value);
            }
            "TB2" => {
                self.tb2 = to_i32(&value);
            }
            "TB3" => {
                self.tb3 = to_i32(&value);
            }
            "TB4" => {
                self.tb4 = to_i32(&value);
            }
            "TB5" => {
                self.tb5 = to_i32(&value);
            }
            "TIOM" => {
                self.tiom = to_i32(&value);
            }
            "TIB0" => {
                self.tib0 = to_i32(&value);
            }
            "TIB1" => {
                self.tib1 = to_i32(&value);
            }
            "TIB2" => {
                self.tib2 = to_i32(&value);
            }
            "TINM" => {
                self.tinm = to_i32(&value);
            }
            "TINB0" => {
                self.tinb0 = to_i32(&value);
            }
            "TINB1" => {
                self.tinb1 = to_i32(&value);
            }
            "TINB2" => {
                self.tinb2 = to_i32(&value);
            }
            "TINB3" => {
                self.tinb3 = to_i32(&value);
            }
            "TSIZ" => {
                self.tsiz = to_i32(&value);
            }
            "TFIL" => {
                self.tfil = to_str(&value);
            }
            "AUCT" => {
                self.auct = to_i32(&value);
            }
            "CNCT" => {
                self.cnct = to_i32(&value);
            }
            "ENBL" => {
                self.enbl = to_i32(&value);
            }
            "VAL" => {
                self.val = to_i32(&value);
            }
            "ERRS" => {
                self.errs = to_str(&value);
            }
            "AQR" => {
                self.aqr = to_i32(&value);
            }
            _ => {
                return Err(CaError::InvalidValue(format!("unknown field: {name}")));
            }
        }
        Ok(())
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 && !self.port.is_empty() {
            self.connect_device();
        }
        Ok(())
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }

        match field {
            // Connection fields → reconnect
            "PORT" | "ADDR" | "DRVINFO" => {
                self.connect_device();
            }

            // Trace mask (numeric) → update bit fields and apply
            "TMSK" => {
                self.update_trace_bits_from_mask();
                self.apply_trace_mask();
            }

            // Trace bit fields → update mask and apply
            "TB0" | "TB1" | "TB2" | "TB3" | "TB4" | "TB5" => {
                self.update_mask_from_trace_bits();
                self.apply_trace_mask();
            }

            // Trace I/O mask (numeric) → update bits and apply
            "TIOM" => {
                self.update_io_bits_from_mask();
                self.apply_trace_io_mask();
            }

            // Trace I/O bit fields → update mask and apply
            "TIB0" | "TIB1" | "TIB2" => {
                self.update_mask_from_io_bits();
                self.apply_trace_io_mask();
            }

            // Trace info mask (numeric) → update bits and apply
            "TINM" => {
                self.update_info_bits_from_mask();
                self.apply_trace_info_mask();
            }

            // Trace info bit fields → update mask and apply
            "TINB0" | "TINB1" | "TINB2" | "TINB3" => {
                self.update_mask_from_info_bits();
                self.apply_trace_info_mask();
            }

            // Trace truncate size
            "TSIZ" => {
                self.apply_trace_truncate_size();
            }

            // Trace file
            "TFIL" => {
                self.apply_trace_file();
            }

            // Enable / disable the entire port (C parity:
            // pasynManager->enable from asynRecord.c:484-486).
            // Forward the typed flag through the port handle so the
            // driver sees `enable()` / `disable()` (and the
            // associated asynExceptionEnable fan-out from
            // PortDriverBase::set_enabled).
            "ENBL" => {
                if let Some(ref entry) = self.port_entry {
                    let _ = entry.handle.set_enable_blocking(self.enbl != 0);
                }
            }

            // Auto-connect (C parity: pasynManager->autoConnect from
            // asynRecord.c:481-482). C fires
            // asynExceptionAutoConnect unconditionally, which Rust
            // mirrors via PortDriverBase::set_auto_connect.
            "AUCT" => {
                if let Some(ref entry) = self.port_entry {
                    let _ = entry.handle.set_auto_connect_blocking(self.auct != 0);
                }
            }

            // CNCT — connect / disconnect the port's *transport*. C
            // `asynCallbackSpecial::callbackConnect` (asynRecord.c:537-539,
            // 857-889): read `pasynManager->isConnected`, and only then call
            // `pasynCommon->connect` (CNCT=1 on a disconnected port) or
            // `pasynCommon->disconnect` (CNCT=0 on a connected one). The
            // isConnected gate is C's, not defensive padding: connecting an
            // already-connected port is an error in the drivers
            // (drvAsynIPPort.c `connectIt`: "already connected"), and CNCT is
            // written by monitorStatus on every cycle, so a re-put of the value
            // the record just read back must not re-drive the wire.
            //
            // This is NOT the record↔port attachment — that is PCNCT below.
            // CNCT used to be a duplicate of it (attach on 1, detach on 0),
            // which left the driver's transport untouched: a CNCT=0 put
            // orphaned the record while the socket stayed open, and CNCT=1
            // could not bring a dropped link back up.
            "CNCT" => {
                let want = self.cnct != 0;
                match self.port_entry {
                    Some(ref entry) => {
                        let handle = entry.handle.clone();
                        match handle.is_connected_blocking() {
                            Ok(is_connected) => {
                                let res = match (want, is_connected) {
                                    (true, false) => Some(("connect", handle.connect_blocking())),
                                    (false, true) => {
                                        Some(("disconnect", handle.disconnect_blocking()))
                                    }
                                    // Already in the requested state: C issues
                                    // no driver call at all.
                                    _ => None,
                                };
                                if let Some((what, Err(e))) = res {
                                    self.errs =
                                        format!("asynCallbackSpecial callbackConnect {what}: {e}");
                                }
                            }
                            Err(e) => {
                                self.errs = format!("asynCallbackSpecial isConnected error: {e}");
                            }
                        }
                    }
                    None => {
                        self.errs = "asynCallbackSpecial isConnected error".to_string();
                    }
                }
                // C monitorStatus re-reads CNCT from isConnected right after
                // (asynRecord.c:1089-1093), so a refused or failed request
                // snaps the field back to the wire's real state instead of
                // leaving the operator's value latched.
                self.refresh_connected_state();
            }

            // PCNCT — attach / detach *this record* to the port. C
            // asynRecord.c:519-527: `connectDevice` on 1;
            // `exceptionCallbackRemove` + `pasynManager->disconnect` +
            // `cancelIOInterruptScan` on 0. The driver's transport is not
            // touched either way.
            "PCNCT" => {
                if self.pcnct != 0 {
                    self.connect_device();
                } else {
                    self.port_entry = None;
                    self.clear_trace_exception_callback();
                    // Detached: `isConnected` has no device to report on, which
                    // is C's CNCT=0 (monitorStatus, :1091-1093).
                    self.refresh_connected_state();
                }
            }

            // Interface change → update validity flags
            "IFACE" => {
                // All interfaces are valid for our port drivers
            }

            // REASON change
            "REASON" => {
                self.resolved_reason = self.reason as usize;
            }

            // --- Serial options ---
            "BAUD" => {
                let rate = menu_index_to_baud_rate(self.baud);
                if rate > 0 {
                    self.lbaud = rate;
                    self.write_option("baud", &rate.to_string());
                }
            }
            "LBAUD" => {
                if self.lbaud > 0 {
                    self.baud = baud_rate_to_menu_index(self.lbaud);
                    self.write_option("baud", &self.lbaud.to_string());
                }
            }
            "PRTY" => {
                let val = match self.prty {
                    1 => "none",
                    2 => "even",
                    3 => "odd",
                    _ => return Ok(()),
                };
                self.write_option("parity", val);
            }
            "DBIT" => {
                let val = match self.dbit {
                    1 => "5",
                    2 => "6",
                    3 => "7",
                    4 => "8",
                    _ => return Ok(()),
                };
                // C parity: `drvAsynSerialPort.c:146/360` recognises
                // the key `"bits"` (not `"csize"`). Rust's serial
                // driver follows the same C key (`serial_port.rs:649`)
                // so the asynRecord DBIT write must use `"bits"` to
                // actually reach the driver — previously routed to
                // `"csize"` which no driver consumes.
                self.write_option("bits", val);
            }
            "SBIT" => {
                let val = match self.sbit {
                    1 => "1",
                    2 => "2",
                    _ => return Ok(()),
                };
                self.write_option("stop", val);
            }
            "MCTL" => {
                let val = match self.mctl {
                    1 => "Y", // CLOCAL
                    2 => "N", // Hardware modem control
                    _ => return Ok(()),
                };
                self.write_option("clocal", val);
            }
            "FCTL" => {
                let val = match self.fctl {
                    1 => "N", // None
                    2 => "Y", // Hardware
                    _ => return Ok(()),
                };
                self.write_option("crtscts", val);
            }
            "IXON" => {
                let val = match self.ixon {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixon", val);
            }
            "IXOFF" => {
                let val = match self.ixoff {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixoff", val);
            }
            "IXANY" => {
                let val = match self.ixany {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixany", val);
            }

            // --- IP options ---
            "HOSTINFO" => {
                if !self.hostinfo.is_empty() {
                    self.write_option("hostinfo", &self.hostinfo.clone());
                }
            }
            "DRTO" => {
                let val = match self.drto {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("disconnectOnReadTimeout", val);
            }

            // GPIB UCMD/ACMD are `pp(TRUE)` with no `special()` in C
            // (asynRecord.dbd:454-467); the command dispatch lives in the
            // process path (asynCallbackProcess, asynRecord.c:819-826), not
            // here. See `process()`.

            // --- AQR (Abort Queue Request) ---
            //
            // C special() for AQR (asynRecord.c:393-408) calls
            // pasynManager->cancelRequest(pasynUser, &wasQueued); only when a
            // request was still queued and is removed (cancelRequest
            // `wasQueued==true`, asynManager.c:1661-1666) does it report
            // "I/O request canceled", raise STATE_ALARM/MAJOR_ALARM and force
            // a completion callback. In every case it then sets
            // state = stateIdle.
            //
            // When this record runs performIO off the scan thread (a
            // can_block port, see `spawn_async_io`), `io_inflight` holds the
            // request's actor CancelToken. The token's state machine reproduces
            // the `wasQueued` split by construction: `cancel()` succeeds only
            // while the phase is still queued (the executor has not yet claimed
            // it with `begin_running`), so a still-queued phase is dropped and
            // `run_io_plan` records the "I/O request canceled" outcome
            // (CANCELED_MSG) — the `wasQueued==true` analogue. A cancel that
            // arrives after the executor began running the phase loses the CAS
            // and is a no-op: the I/O completes and applies normally, matching
            // C `cancelRequest` returning `wasQueued==0` once the callback is
            // active (asynManager.c:1645-1659). The completion re-entry is the
            // single owner that applies the outcome to ERRS and clears
            // `io_inflight` (the `state = stateIdle` transition), so AQR must
            // NOT touch `io_inflight` or supersede the re-entry token here: the
            // completion token is minted post-I/O and is always current
            // (mint advances the generation), so `cancel_async_reentry` could
            // only strand the record by racing the mint, never suppress it.
            // STATE_ALARM/MAJOR_ALARM is not modeled (this record reports I/O
            // errors via ERRS only, like its octet path).
            //
            // With no request in flight (synchronous port, or already
            // completed) AQR is the C `wasQueued==false` idle no-op.
            "AQR" => {
                if let Some(inflight) = &self.io_inflight {
                    inflight.cancel.cancel();
                }
            }

            // --- EOS (end-of-string) delimiters ---
            //
            // C parity: `asynRecord.c::monitor` (the `OEOS`/`IEOS`
            // special-write path at lines 374-393) decodes the
            // backslash-escaped DB field via `dbTranslateEscape`
            // and calls `pasynOctet->setOutputEos /
            // setInputEos(pasynUser, eos, eos_len)`. Previously
            // Rust routed through `set_option_blocking("oeos", ...)`
            // which lands in `PortDriverBase::options` — no driver
            // consumes the `oeos`/`ieos` keys, so the EOS interpose
            // ignores the asynRecord write. The actor-routed
            // `SetInputEos`/`SetOutputEos` ops drive the driver
            // trait hooks (`set_input_eos` / `set_output_eos`) so
            // the value reaches `PortDriverBase::input_eos /
            // output_eos`, which is what the EOS interpose reads.
            "OEOS" => {
                let bytes = translate_escape(&self.oeos);
                if let Some(ref entry) = self.port_entry {
                    if let Err(e) = entry.handle.set_output_eos_blocking(&bytes) {
                        self.errs = format!("set_output_eos: {e}");
                    }
                }
            }
            "IEOS" => {
                let bytes = translate_escape(&self.ieos);
                if let Some(ref entry) = self.port_entry {
                    if let Err(e) = entry.handle.set_input_eos_blocking(&bytes) {
                        self.errs = format!("set_input_eos: {e}");
                    }
                }
            }

            // --- UI32MASK change ---
            "UI32MASK" => {
                // Just record the value, used during I/O
            }

            _ => {}
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Completion re-entry of a non-blocking I/O cycle: the off-thread
        // orchestration filled the result slot and fired the async-record
        // token, re-entering here. This is C's `process()` `pact==TRUE`
        // branch (asynRecord.c:362-363) — apply the I/O results the port
        // thread produced and finish (`state = stateIdle`). Checked first so
        // a completion never re-issues I/O.
        if let Some(inflight) = self.io_inflight.take() {
            let outcome = inflight.result.lock().unwrap().take().unwrap_or_default();
            self.apply_io_outcome(outcome);
            return Ok(ProcessOutcome::complete());
        }

        // Re-import the trace masks if an external `setTrace*` fired since the
        // last cycle. C refreshes these readback fields from its
        // `exceptCallback` immediately (asynRecord.c:903-917). When the record
        // carries a database handle the subscription
        // (`register_trace_exception_callback`) does the same — it posts the
        // fields out of band — and never sets this flag. This dirty path is
        // the fallback for a record with no handle / runtime, draining through
        // the single `read_trace_state` owner.
        if self.trace_status_dirty.swap(false, Ordering::AcqRel) {
            self.read_trace_state();
        }

        // C asynCallbackProcess (asynRecord.c:819-827) dispatches by
        // priority: a pending UCMD universal GPIB command first, else a
        // pending ACMD addressed GPIB command, else octet/register I/O
        // (performIO). The two GPIB commands take priority over performIO
        // — they run even when TMOD is NoIO — and each resets its menu
        // field back to None so the operator's request is consumed
        // exactly once.
        //
        // gpibUniversalCmd / gpibAddressedCmd (asynRecord.c:1647-1651,
        // 1693-1697) begin with `if (!pasynRec->gpibiv)`: with no asynGpib
        // interface they report "No asynGpib interface", raise a
        // COMM_ALARM/MAJOR_ALARM, and return without touching the bus.
        // epics-rs ports carry no asynGpib interface (GPIBIV is always 0,
        // set in connect_device), so that no-interface branch is the only
        // reachable path here. The COMM/MAJOR severity is staged in `io_alarm`
        // and committed by `check_alarms` on this same cycle.
        if self.ucmd != 0 {
            self.errs = "No asynGpib interface".to_string();
            self.io_alarm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Major));
            self.ucmd = 0;
            return Ok(ProcessOutcome::complete());
        }
        if self.acmd != 0 {
            self.errs = "No asynGpib interface".to_string();
            self.io_alarm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Major));
            self.acmd = 0;
            return Ok(ProcessOutcome::complete());
        }

        let tmod = TransferMode::from_u16(self.tmod as u16);
        if tmod == TransferMode::NoIo {
            return Ok(ProcessOutcome::complete());
        }

        self.errs.clear();

        // C `process()` (asynRecord.c:342-353) queues `performIO`, then
        // `canBlock(&yesNo)`: a blocking port runs the I/O on the port
        // thread (`pact = TRUE; return`) and the record completes on the
        // callback re-process; a non-blocking port runs it inline (`goto
        // done`). Mirror that split — a `can_block` port with a live
        // database handle submits the I/O off the scan thread and re-enters
        // on completion; everything else (non-blocking port, or a record
        // not built into a database) keeps the synchronous inline path.
        let blocking_handle = self
            .port_entry
            .as_ref()
            .and_then(|e| e.handle.can_block().then(|| e.handle.clone()));
        if let (Some(handle), Some((name, db))) = (blocking_handle, self.async_ctx.clone()) {
            return Ok(self.spawn_async_io(handle, name, db));
        }

        self.perform_io()?;
        Ok(ProcessOutcome::complete())
    }

    fn clears_udf(&self) -> bool {
        true
    }
}

impl Drop for AsynRecord {
    fn drop(&mut self) {
        // C removes `exceptCallback` when the record disconnects
        // (asynRecord.c:523,1154,1313); mirror that on teardown so a
        // dropped record leaves no dangling subscription in the
        // ExceptionManager callback list.
        self.clear_trace_exception_callback();
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use epics_base_rs::server::record::RecordProcessResult;

    #[test]
    fn test_default_fields() {
        let rec = AsynRecord::default();
        assert_eq!(rec.record_type(), "asyn");
        assert_eq!(rec.cnct, 0);
        assert_eq!(rec.tmot, 1.0);
        assert_eq!(rec.omax, 80);
        assert_eq!(rec.imax, 80);
        assert_eq!(rec.tsiz, 80);
        assert_eq!(rec.ui32mask, 0xFFFFFFFF);
        assert_eq!(rec.auct, 1);
        assert_eq!(rec.enbl, 1);
    }

    #[test]
    fn test_field_list_count() {
        let rec = AsynRecord::default();
        assert_eq!(rec.field_list().len(), 76);
    }

    /// Every asynRecord `DBF_MENU` field must (a) serve enum choice strings
    /// and (b) hold its value as a `Short` menu index, so the framework
    /// promotes it to `DBR_ENUM`. Regression guard for the blank
    /// choice-button / menu widget symptom (field served as a plain `Short`
    /// with no states). The 34 names are the full `field(...,DBF_MENU)` set
    /// in asynRecord.dbd.
    #[test]
    fn menu_fields_have_choices_and_short_values() {
        let rec = AsynRecord::default();
        const MENU_FIELDS: &[&str] = &[
            "TMOD", "IFACE", "OFMT", "IFMT", "EOMR", "TB0", "TB1", "TB2", "TB3", "TB4", "TB5",
            "TIB0", "TIB1", "TIB2", "TINB0", "TINB1", "TINB2", "TINB3", "AUCT", "CNCT", "PCNCT",
            "ENBL", "BAUD", "PRTY", "DBIT", "SBIT", "MCTL", "FCTL", "IXON", "IXOFF", "IXANY",
            "DRTO", "UCMD", "ACMD",
        ];
        assert_eq!(MENU_FIELDS.len(), 34, "asynRecord has 34 DBF_MENU fields");
        for f in MENU_FIELDS {
            let choices = rec.menu_field_choices(f);
            assert!(choices.is_some(), "{f} must serve menu choices");
            assert!(
                !choices.unwrap().is_empty(),
                "{f} choices must be non-empty"
            );
            assert!(
                matches!(rec.get_field(f), Some(EpicsValue::Short(_))),
                "{f} value must be a Short menu index for DBR_ENUM promotion"
            );
        }
    }

    /// Exact choice strings for representative menus, verbatim from
    /// asynRecord.dbd — catches a typo, reordering, or renamed choice.
    #[test]
    fn menu_choice_strings_match_dbd() {
        let rec = AsynRecord::default();
        assert_eq!(rec.menu_field_choices("TB0"), Some(&["Off", "On"][..]));
        assert_eq!(
            rec.menu_field_choices("IFACE"),
            Some(&["asynOctet", "asynInt32", "asynUInt32Digital", "asynFloat64"][..])
        );
        assert_eq!(
            rec.menu_field_choices("CNCT"),
            Some(&["Disconnect", "Connect"][..])
        );
        assert_eq!(
            rec.menu_field_choices("SBIT"),
            Some(&["Unknown", "1", "2"][..])
        );
        // serialBAUD has 16 entries; check length + both boundaries.
        let baud = rec.menu_field_choices("BAUD").unwrap();
        assert_eq!(baud.len(), 16);
        assert_eq!(baud[0], "Unknown");
        assert_eq!(baud[15], "1152000");
    }

    /// Plain numeric / string fields (text-entry widgets) must NOT be
    /// promoted to enum — they keep their number/string wire form.
    #[test]
    fn non_menu_fields_have_no_choices() {
        let rec = AsynRecord::default();
        for f in [
            "PORT", "ADDR", "REASON", "TMSK", "TIOM", "TINM", "TSIZ", "LBAUD", "ERRS",
        ] {
            assert_eq!(rec.menu_field_choices(f), None, "{f} is not a menu field");
        }
    }

    #[test]
    fn test_get_put_roundtrip() {
        let mut rec = AsynRecord::default();
        rec.put_field("PORT", EpicsValue::String("SIM1".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("PORT"),
            Some(EpicsValue::String("SIM1".into()))
        );

        rec.put_field("ADDR", EpicsValue::Long(3)).unwrap();
        assert_eq!(rec.get_field("ADDR"), Some(EpicsValue::Long(3)));

        rec.put_field("TMOT", EpicsValue::Double(2.5)).unwrap();
        assert_eq!(rec.get_field("TMOT"), Some(EpicsValue::Double(2.5)));

        rec.put_field("F64OUT", EpicsValue::Double(3.14)).unwrap();
        assert_eq!(rec.get_field("F64OUT"), Some(EpicsValue::Double(3.14)));
    }

    #[test]
    fn test_trace_bit_sync() {
        let mut rec = AsynRecord::default();

        // Set TMSK → bits should update
        rec.tmsk = (TraceMask::ERROR | TraceMask::FLOW).bits() as i32;
        rec.update_trace_bits_from_mask();
        assert_eq!(rec.tb0, 1); // ERROR
        assert_eq!(rec.tb4, 1); // FLOW
        assert_eq!(rec.tb1, 0);
        assert_eq!(rec.tb2, 0);
        assert_eq!(rec.tb3, 0);
        assert_eq!(rec.tb5, 0);

        // Set bits → mask should update
        rec.tb0 = 1;
        rec.tb1 = 1;
        rec.tb2 = 0;
        rec.tb3 = 0;
        rec.tb4 = 0;
        rec.tb5 = 1;
        rec.update_mask_from_trace_bits();
        let expected = TraceMask::ERROR | TraceMask::IO_DEVICE | TraceMask::WARNING;
        assert_eq!(rec.tmsk, expected.bits() as i32);
    }

    #[test]
    fn test_io_bit_sync() {
        let mut rec = AsynRecord::default();

        rec.tiom = (TraceIoMask::ASCII | TraceIoMask::HEX).bits() as i32;
        rec.update_io_bits_from_mask();
        assert_eq!(rec.tib0, 1); // ASCII
        assert_eq!(rec.tib1, 0); // ESCAPE
        assert_eq!(rec.tib2, 1); // HEX
    }

    #[test]
    fn test_info_bit_sync() {
        let mut rec = AsynRecord::default();

        rec.tinm = (TraceInfoMask::TIME | TraceInfoMask::THREAD).bits() as i32;
        rec.update_info_bits_from_mask();
        assert_eq!(rec.tinb0, 1); // TIME
        assert_eq!(rec.tinb1, 0); // PORT
        assert_eq!(rec.tinb2, 0); // SOURCE
        assert_eq!(rec.tinb3, 1); // THREAD
    }

    #[test]
    fn test_connect_nonexistent_port() {
        let mut rec = AsynRecord::default();
        rec.port = "NONEXISTENT".to_string();
        rec.connect_device();
        assert_eq!(rec.cnct, 0);
        assert!(rec.errs.contains("not found"));
    }

    #[test]
    fn test_connect_empty_port() {
        let mut rec = AsynRecord::default();
        rec.connect_device();
        assert_eq!(rec.cnct, 0);
        assert!(rec.port_entry.is_none());
    }

    #[test]
    fn test_process_no_io_mode() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::NoIo as i32;
        let result = rec.process().unwrap();
        assert_eq!(result.result, RecordProcessResult::Complete);
    }

    #[test]
    fn test_process_not_connected() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.process().unwrap();
        assert_eq!(rec.errs, "not connected");
    }

    #[test]
    fn process_ucmd_with_no_gpib_interface_errors_and_resets() {
        // C asynCallbackProcess (asynRecord.c:819-822): a pending UCMD is
        // dispatched to gpibUniversalCmd then reset to None. With no
        // asynGpib interface (the only epics-rs case) gpibUniversalCmd
        // reports "No asynGpib interface" (asynRecord.c:1648). The command
        // takes priority over performIO, so even with TMOD=Read on an
        // unconnected record the I/O path ("not connected") is never
        // reached.
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.ucmd = 1; // Device Clear (DCL)
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynGpib interface");
        assert_eq!(rec.ucmd, 0, "UCMD must reset to None after dispatch");
    }

    #[test]
    fn process_acmd_with_no_gpib_interface_errors_and_resets() {
        // C asynCallbackProcess (asynRecord.c:823-826): a pending ACMD is
        // dispatched to gpibAddressedCmd then reset to None. With no
        // asynGpib interface gpibAddressedCmd reports "No asynGpib
        // interface" (asynRecord.c:1694).
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.acmd = 1; // Group Execute Trigger (GET)
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynGpib interface");
        assert_eq!(rec.acmd, 0, "ACMD must reset to None after dispatch");
    }

    #[test]
    fn process_ucmd_takes_priority_over_acmd() {
        // C dispatches UCMD first (`if`), ACMD only in the `else if`. With
        // both pending only UCMD is consumed this cycle; ACMD is left for
        // the next process.
        let mut rec = AsynRecord::default();
        rec.ucmd = 1;
        rec.acmd = 1;
        rec.process().unwrap();
        assert_eq!(rec.ucmd, 0, "UCMD consumed first");
        assert_eq!(rec.acmd, 1, "ACMD left pending while UCMD was set");
    }

    #[test]
    fn test_special_trace_mask() {
        let mut rec = AsynRecord::default();
        rec.tmsk = (TraceMask::ERROR | TraceMask::WARNING | TraceMask::FLOW).bits() as i32;
        rec.special("TMSK", true).unwrap();
        assert_eq!(rec.tb0, 1); // ERROR
        assert_eq!(rec.tb4, 1); // FLOW
        assert_eq!(rec.tb5, 1); // WARNING
    }

    #[test]
    fn test_special_trace_bits() {
        let mut rec = AsynRecord::default();
        rec.tb0 = 1;
        rec.tb3 = 1;
        rec.special("TB0", true).unwrap();
        assert_eq!(
            rec.tmsk as u32,
            (TraceMask::ERROR | TraceMask::IO_DRIVER).bits()
        );
    }

    #[test]
    fn test_register_and_get_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct TestDriver(PortDriverBase);
        impl TestDriver {
            fn new() -> Self {
                Self(PortDriverBase::new(
                    "test_asyn_rec",
                    1,
                    PortFlags::default(),
                ))
            }
        }
        impl PortDriver for TestDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(TestDriver::new()), rx);
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, "test_asyn_rec".into(), interrupts);
        let trace = Arc::new(TraceManager::new());

        register_port("test_asyn_rec", handle, trace);

        let entry = registry::get_port("test_asyn_rec");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().handle.port_name(), "test_asyn_rec");
    }

    /// Regression.
    ///
    /// On a multi-device port a record with ADDR >= 0 must route trace
    /// controls to the (PORT,ADDR) device trace state, not the port-wide
    /// state (C findTracePvt, asynManager.c:541-549). A record adjusting
    /// device 3 must not change device 4 or the port default.
    #[test]
    fn trace_controls_route_to_device_on_multi_device_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct MdDriver(PortDriverBase);
        impl PortDriver for MdDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_trace_addr_md";
        let flags = PortFlags {
            multi_device: true,
            ..PortFlags::default()
        };
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(MdDriver(PortDriverBase::new(port_name, 4, flags))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let mut handle = PortHandle::new(tx, port_name.into(), interrupts);
        handle.set_capabilities(true, 4);
        let trace = Arc::new(TraceManager::new());
        register_port(port_name, handle, trace.clone());

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.addr = 3;
        rec.connect_device();
        assert_eq!(rec.cnct, 1);
        assert!(rec.trace_addr_target() == Some(3));

        // Apply a device-3 trace mask through the record's TMSK path.
        rec.tmsk = (TraceMask::ERROR | TraceMask::FLOW).bits() as i32;
        rec.apply_trace_mask();
        rec.tsiz = 17;
        rec.apply_trace_truncate_size();

        // Device 3 sees FLOW; device 4 and the port default do not.
        assert!(trace.is_enabled_device(port_name, 3, TraceMask::FLOW));
        assert!(!trace.is_enabled_device(port_name, 4, TraceMask::FLOW));
        assert!(!trace.is_enabled(port_name, TraceMask::FLOW));

        // A single-device (addr 0) record on the same port targets the port.
        let mut rec0 = AsynRecord::default();
        rec0.port = port_name.to_string();
        rec0.addr = -1; // unaddressed -> port-wide
        rec0.connect_device();
        assert!(rec0.trace_addr_target().is_none());
    }

    /// Regression (connect-time import).
    ///
    /// C monitorStatus (asynRecord.c:1079-1084) imports the trace info mask
    /// into TINM/TINB0..3 on connect; previously Rust read only the trace
    /// mask and I/O mask, so a record connecting after a non-default
    /// asynSetTraceInfoMask showed TINM/TINB* as zero.
    #[test]
    fn read_trace_state_imports_info_mask_on_connect() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct D(PortDriverBase);
        impl PortDriver for D {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_trace_info_sync";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(port_name, 1, PortFlags::default()))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        let trace = Arc::new(TraceManager::new());
        // Non-default info mask set on the manager BEFORE the record connects.
        trace.set_trace_info_mask(
            Some(port_name),
            TraceInfoMask::SOURCE | TraceInfoMask::THREAD,
        );
        register_port(port_name, handle, trace);

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device();
        assert_eq!(rec.cnct, 1);

        assert_eq!(
            rec.tinm as u32,
            (TraceInfoMask::SOURCE | TraceInfoMask::THREAD).bits()
        );
        assert_eq!(rec.tinb0, 0); // TIME
        assert_eq!(rec.tinb1, 0); // PORT
        assert_eq!(rec.tinb2, 1); // SOURCE
        assert_eq!(rec.tinb3, 1); // THREAD
    }

    /// A trace info mask changed externally AFTER the record connected must
    /// reach the record's TINM/TINB* fields. C delivers this through
    /// `exceptCallback` -> `monitorStatus` (asynRecord.c:903-917); epics-rs
    /// flags the change in a trace exception callback and re-imports it on
    /// the next `process()` via `read_trace_state`.
    #[test]
    fn external_trace_info_mask_reflected_after_process() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct D(PortDriverBase);
        impl PortDriver for D {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_trace_info_live";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(port_name, 1, PortFlags::default()))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        let trace = Arc::new(TraceManager::new());
        // Wire the exception sink as a real IOC would (PortManager installs
        // it); without it the record's subscription is a no-op.
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::TIME);
        register_port(port_name, handle, trace.clone());

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32; // process() does no I/O
        rec.connect_device();
        assert_eq!(rec.cnct, 1);
        assert_eq!(rec.tinm as u32, TraceInfoMask::TIME.bits());
        assert_eq!(rec.tinb0, 1); // TIME
        assert_eq!(rec.tinb1, 0); // PORT

        // External reconfiguration after connect.
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::PORT | TraceInfoMask::THREAD);
        // Stale until the record runs again (no async record-side post).
        assert_eq!(rec.tinm as u32, TraceInfoMask::TIME.bits());

        rec.process().unwrap();

        assert_eq!(
            rec.tinm as u32,
            (TraceInfoMask::PORT | TraceInfoMask::THREAD).bits()
        );
        assert_eq!(rec.tinb0, 0); // TIME cleared
        assert_eq!(rec.tinb1, 1); // PORT
        assert_eq!(rec.tinb3, 1); // THREAD
    }

    /// Regression.
    ///
    /// C asynRecord stores a device octet read into the single IFMT-selected
    /// input field — ASCII into AINP, Binary/Hybrid into the BINP byte buffer
    /// (`asynRecord.c:1503-1509`) — and the monitor path posts only that field
    /// plus the always-escaped TINP (`asynRecord.c:1012-1018`). Pre-fix Rust
    /// set AINP (lossy UTF-8), TINP, and BINP on every read regardless of
    /// IFMT, so a client watching the unselected field saw values and changes
    /// Base never publishes.
    #[test]
    fn octet_read_updates_only_ifmt_selected_field() {
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        // A non-UTF8 leading byte makes the AINP lossy text observably
        // different from the raw BINP bytes.
        const PAYLOAD: &[u8] = &[0xFF, b'A', b'B'];

        struct OctetDriver(PortDriverBase);
        impl PortDriver for OctetDriver {
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
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                let n = PAYLOAD.len().min(buf.len());
                buf[..n].copy_from_slice(&PAYLOAD[..n]);
                Ok((n, EomReason::END))
            }
        }

        let port_name = "test_ifmt_octet_read";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(OctetDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));

        // ASCII read: AINP gets the (lossy) text; BINP must stay untouched.
        let mut ascii = AsynRecord::default();
        ascii.port = port_name.to_string();
        ascii.connect_device();
        ascii.iface = 0; // asynOctet
        ascii.tmod = TransferMode::Read as i32;
        ascii.imax = 256;
        ascii.ifmt = ASYN_FMT_ASCII;
        ascii.binp = b"SENTINEL".to_vec();
        ascii.process().unwrap();
        assert_eq!(ascii.errs, "");
        assert_eq!(ascii.nord, PAYLOAD.len() as i32);
        assert_eq!(ascii.ainp, String::from_utf8_lossy(PAYLOAD));
        assert_eq!(
            ascii.binp,
            b"SENTINEL".to_vec(),
            "ASCII read must not touch BINP"
        );
        assert!(!ascii.tinp.is_empty(), "TINP is posted for every read mode");

        // Binary read: BINP gets the raw bytes; AINP must stay untouched.
        let mut binary = AsynRecord::default();
        binary.port = port_name.to_string();
        binary.connect_device();
        binary.iface = 0;
        binary.tmod = TransferMode::Read as i32;
        binary.imax = 256;
        binary.ifmt = ASYN_FMT_BINARY;
        binary.ainp = "SENTINEL".to_string();
        binary.process().unwrap();
        assert_eq!(binary.errs, "");
        assert_eq!(binary.nord, PAYLOAD.len() as i32);
        assert_eq!(binary.binp, PAYLOAD.to_vec());
        assert_eq!(binary.ainp, "SENTINEL", "Binary read must not touch AINP");
    }

    /// C parity for `performOctetIO` on an I/O *error* (asynRecord.c:1547,
    /// :1560-1631). C `memset(inptr,0,inlen)` before the read and assigns
    /// `nawt`/`nord`/`eomr`/`tinp` from the (zero-on-error) transfer
    /// unconditionally — the error branch does not skip them — so a failed
    /// transfer lands zero/empty input fields, not the prior transfer's stale
    /// values. asyn-rs surfaces a failed octet read/write as `Err` carrying no
    /// bytes; the result-recorders must mirror the same zero transfer.
    #[test]
    fn octet_error_resets_transfer_fields() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct FailingOctetDriver(PortDriverBase);
        impl PortDriver for FailingOctetDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "read boom".into(),
                })
            }
            fn io_write_octet(
                &mut self,
                _user: &mut AsynUser,
                _data: &[u8],
            ) -> crate::error::AsynResult<usize> {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "write boom".into(),
                })
            }
        }

        let port_name = "test_octet_error_reset";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(FailingOctetDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));

        // ASCII read failure: NORD/EOMR cleared to 0, AINP/TINP cleared to "",
        // and BINP (not the IFMT-selected field) left as it was — matching C,
        // which memsets only `inptr` (the ASCII buffer here).
        let mut ascii = AsynRecord::default();
        ascii.port = port_name.to_string();
        ascii.connect_device();
        ascii.iface = 0; // asynOctet
        ascii.tmod = TransferMode::Read as i32;
        ascii.imax = 256;
        ascii.ifmt = ASYN_FMT_ASCII;
        ascii.nord = 5;
        ascii.eomr = 2;
        ascii.ainp = "STALE".to_string();
        ascii.tinp = "STALE".to_string();
        ascii.binp = b"KEEP".to_vec();
        ascii.process().unwrap();
        assert!(!ascii.errs.is_empty(), "read error must set ERRS");
        assert_eq!(ascii.nord, 0, "failed read must reset NORD to 0");
        assert_eq!(ascii.eomr, 0, "failed read must reset EOMR to 0");
        assert_eq!(ascii.ainp, "", "failed ASCII read must clear AINP");
        assert_eq!(ascii.tinp, "", "failed read must clear TINP");
        assert_eq!(
            ascii.binp,
            b"KEEP".to_vec(),
            "ASCII path must not touch BINP"
        );

        // Binary read failure: BINP cleared, AINP (not selected) untouched.
        let mut binary = AsynRecord::default();
        binary.port = port_name.to_string();
        binary.connect_device();
        binary.iface = 0;
        binary.tmod = TransferMode::Read as i32;
        binary.imax = 256;
        binary.ifmt = ASYN_FMT_BINARY;
        binary.nord = 7;
        binary.eomr = 3;
        binary.binp = b"STALE".to_vec();
        binary.ainp = "KEEP".to_string();
        binary.tinp = "STALE".to_string();
        binary.process().unwrap();
        assert!(!binary.errs.is_empty(), "read error must set ERRS");
        assert_eq!(binary.nord, 0, "failed read must reset NORD to 0");
        assert_eq!(binary.eomr, 0, "failed read must reset EOMR to 0");
        assert_eq!(
            binary.binp,
            Vec::<u8>::new(),
            "failed binary read must clear BINP"
        );
        assert_eq!(binary.tinp, "", "failed read must clear TINP");
        assert_eq!(binary.ainp, "KEEP", "Binary path must not touch AINP");

        // Write failure: NAWT reset to 0 (C assigns nbytesTransfered=0).
        let mut writer = AsynRecord::default();
        writer.port = port_name.to_string();
        writer.connect_device();
        writer.iface = 0;
        writer.tmod = TransferMode::Write as i32;
        writer.ofmt = ASYN_FMT_ASCII;
        writer.aout = "hello".to_string();
        writer.nawt = 9;
        writer.process().unwrap();
        assert!(!writer.errs.is_empty(), "write error must set ERRS");
        assert_eq!(writer.nawt, 0, "failed write must reset NAWT to 0");
    }

    /// R7-46: a device that emits a partial line and then goes quiet reaches
    /// the record as `asynTimeout` **plus** the bytes it did send. C
    /// `performOctetIO` assigns `eomr`/`nord`/`inptr`/`tinp` from
    /// `nbytesTransfered` regardless of the read status (asynRecord.c:1591-1629
    /// — the error branch at :1583 only reports + alarms), and the EOS
    /// interpose publishes that count with the failing status
    /// (asynInterposeEos.c:242-253). So AINP="abc", NORD=3, EOMR=0 land
    /// together with READ_ALARM/MAJOR.
    ///
    /// Before the fix the transfer rode only in the driver's buffer and the
    /// `?` at the actor dispatch dropped it: the record saw the alarm with
    /// AINP="", NORD=0.
    #[test]
    fn octet_partial_read_delivers_bytes_with_the_timeout() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::{EomReason, PartialOctetRead};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::{AlarmSeverity, CommonFields};
        use tokio::sync::mpsc;

        /// The EOS interpose's output for "device sent `abc`, then nothing
        /// until the timeout": the bytes are in the caller's buffer AND on
        /// the error (`asynInterposeEos.c:242-253`).
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
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                buf[..3].copy_from_slice(b"abc");
                buf[3] = 0;
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

        let port_name = "test_octet_partial_read";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(PartialThenTimeout(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device();
        rec.iface = 0; // asynOctet
        rec.tmod = TransferMode::Read as i32;
        rec.imax = 256;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.process().unwrap();

        assert_eq!(rec.ainp, "abc", "the partial line reaches AINP (C :1503)");
        assert_eq!(rec.nord, 3, "NORD = nbytesTransfered (C :1627)");
        assert_eq!(rec.eomr, 0, "no EOS matched, buffer never filled (C :1591)");
        assert_eq!(rec.tinp, "abc", "TINP is posted for the failed read too");
        assert!(!rec.errs.is_empty(), "the timeout still reports into ERRS");

        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::READ_ALARM, "C :1599 recGblSetSevr");
        assert_eq!(c.nsev, AlarmSeverity::Major, "READ_ALARM is MAJOR");

        // Binary IFMT takes the same transfer through BINP.
        let mut bin = AsynRecord::default();
        bin.port = port_name.to_string();
        bin.connect_device();
        bin.iface = 0;
        bin.tmod = TransferMode::Read as i32;
        bin.imax = 256;
        bin.ifmt = ASYN_FMT_BINARY;
        bin.process().unwrap();
        assert_eq!(bin.binp, b"abc".to_vec(), "partial bytes reach BINP too");
        assert_eq!(bin.nord, 3);
    }

    /// R8-51: CNCT drives the *driver's transport*, PCNCT the record↔port
    /// attachment. C `special` routes a CNCT put to `asynCallbackSpecial`
    /// (asynRecord.c:537-539), whose `callbackConnect` (:857-889) reads
    /// `pasynManager->isConnected` and then calls `pasynCommon->connect` or
    /// `->disconnect` — while PCNCT (:519-527) only runs `connectDevice` /
    /// `pasynManager->disconnect`, never touching the wire. And CNCT is a
    /// *readback*: `monitorStatus` (:1089-1093) assigns it from `isConnected`.
    ///
    /// Before the fix CNCT was a duplicate of PCNCT: a CNCT=0 put detached the
    /// record and left the socket open, CNCT=1 could not raise a dropped link,
    /// and `connect_device` latched CNCT=1 on a mere attach — so a
    /// registered-but-disconnected port reported a live wire.
    #[test]
    fn cnct_drives_the_transport_and_pcnct_the_attachment() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::mpsc;

        /// Counts the transport calls C would make through `pasynCommon`.
        struct CountingDriver {
            base: PortDriverBase,
            connects: Arc<AtomicUsize>,
            disconnects: Arc<AtomicUsize>,
        }
        impl PortDriver for CountingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> crate::error::AsynResult<()> {
                self.connects.fetch_add(1, Ordering::SeqCst);
                self.base.set_connected(true);
                Ok(())
            }
            fn disconnect(&mut self, _user: &AsynUser) -> crate::error::AsynResult<()> {
                self.disconnects.fetch_add(1, Ordering::SeqCst);
                self.base.set_connected(false);
                Ok(())
            }
        }

        let port_name = "test_cnct_transport";
        let connects = Arc::new(AtomicUsize::new(0));
        let disconnects = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(CountingDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                connects: connects.clone(),
                disconnects: disconnects.clone(),
            }),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), Arc::new(InterruptManager::new(256)));
        register_port(port_name, handle, Arc::new(TraceManager::new()));

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device();
        // The actor auto-connected the port at startup; CNCT reads that back
        // rather than latching on the attach.
        assert_eq!(rec.cnct, 1, "CNCT is the port's transport state");
        assert_eq!(rec.pcnct, 1, "PCNCT is the attachment");
        let base_connects = connects.load(Ordering::SeqCst);

        // CNCT=0 → pasynCommon->disconnect. The record stays attached.
        rec.cnct = 0;
        rec.special("CNCT", true).unwrap();
        assert_eq!(
            disconnects.load(Ordering::SeqCst),
            1,
            "CNCT=0 must disconnect the driver's transport"
        );
        assert_eq!(rec.cnct, 0, "readback follows the wire");
        assert_eq!(
            rec.pcnct, 1,
            "CNCT must not detach the record (that is PCNCT)"
        );
        assert!(
            rec.port_entry.is_some(),
            "CNCT must not drop the port binding"
        );

        // CNCT=1 on a disconnected port → pasynCommon->connect.
        rec.cnct = 1;
        rec.special("CNCT", true).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            base_connects + 1,
            "CNCT=1 must connect the driver's transport"
        );
        assert_eq!(rec.cnct, 1);

        // C's isConnected gate: re-putting the state the port is already in
        // issues no driver call at all (asynRecord.c:865-882).
        rec.cnct = 1;
        rec.special("CNCT", true).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            base_connects + 1,
            "an already-connected port must not be re-connected"
        );

        // PCNCT=0 detaches the record and leaves the transport alone.
        let disconnects_before = disconnects.load(Ordering::SeqCst);
        rec.pcnct = 0;
        rec.special("PCNCT", true).unwrap();
        assert!(rec.port_entry.is_none(), "PCNCT=0 detaches the record");
        assert_eq!(
            disconnects.load(Ordering::SeqCst),
            disconnects_before,
            "PCNCT must not touch the driver's transport"
        );
        assert_eq!(
            rec.cnct, 0,
            "detached: isConnected has no device to report on (C :1091)"
        );
    }

    /// R8-48: NAWT is what the *device* took, on both arms. C `performOctetIO`
    /// (asynRecord.c:1524-1556) seeds `nbytesTransfered = 0`, hands it to the
    /// octet chain — which fills it in on success and on failure alike
    /// (`drvAsynSerialPort.c:849` writes `numchars - nleft` on the timeout
    /// break) — and commits it with `nawt = nbytesTransfered` (:1547) *before*
    /// testing the status (:1551). The short-write diagnostic then fires
    /// whenever the status failed **or** fewer bytes went out than were asked
    /// for, landing "Write error, nout=%d, %s" in ERRS via reportError.
    ///
    /// Before the fix, the Ok arm published the *planned* length (ignoring the
    /// reply's real count), the Err arm hard-coded 0, and a success-status
    /// short write raised nothing at all.
    #[test]
    fn octet_write_reports_the_transferred_count_on_both_arms() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use epics_base_rs::server::record::{AlarmSeverity, CommonFields};
        use tokio::sync::mpsc;

        /// A port that takes `accept` bytes of every write and then reports
        /// `outcome` — the shape of a device whose buffer filled mid-command.
        struct ShortWriteDriver {
            base: PortDriverBase,
            accept: usize,
            fail: bool,
        }
        impl PortDriver for ShortWriteDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(
                &mut self,
                _user: &mut AsynUser,
                _data: &[u8],
            ) -> crate::error::AsynResult<usize> {
                if self.fail {
                    Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "serial write timeout".into(),
                    }
                    .with_partial_write(self.accept))
                } else {
                    Ok(self.accept)
                }
            }
        }

        fn writer(port_name: &'static str, accept: usize, fail: bool) -> AsynRecord {
            let interrupts = Arc::new(InterruptManager::new(256));
            let (tx, rx) = mpsc::channel(256);
            let actor = PortActor::new(
                Box::new(ShortWriteDriver {
                    base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                    accept,
                    fail,
                }),
                rx,
            );
            std::thread::spawn(move || actor.run());
            let handle = PortHandle::new(tx, port_name.into(), interrupts);
            register_port(port_name, handle, Arc::new(TraceManager::new()));

            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            rec.connect_device();
            rec.iface = 0; // asynOctet
            rec.tmod = TransferMode::Write as i32;
            rec.ofmt = ASYN_FMT_ASCII;
            rec.aout = "hello".to_string(); // nwrite = 5
            rec.nawt = 99; // stale count from a previous cycle
            rec
        }

        // Failing write that moved bytes first: NAWT is the count the device
        // took, not 0, and ERRS carries C's nout diagnostic.
        let mut partial = writer("test_short_write_err", 3, true);
        partial.process().unwrap();
        assert_eq!(partial.nawt, 3, "NAWT = nbytesTransfered (C :1547)");
        assert!(
            partial.errs.starts_with("Write error, nout=3,"),
            "C :1552 reportError text, got {:?}",
            partial.errs
        );
        // C's octet write branch reports but raises NO severity (:1551-1555) —
        // unlike the register writes, which recGblSetSevr(WRITE_ALARM, MAJOR).
        let mut c = CommonFields::default();
        partial.check_alarms(&mut c);
        assert_eq!(
            c.nsev,
            AlarmSeverity::NoAlarm,
            "octet write error raises no record severity"
        );

        // Short write that reported *success*: C still fires the diagnostic
        // (`nbytesTransfered != nwrite`), and NAWT is the driver's real count,
        // not the planned length.
        let mut short_ok = writer("test_short_write_ok", 2, false);
        short_ok.process().unwrap();
        assert_eq!(short_ok.nawt, 2, "NAWT comes from the reply, not from OPTR");
        assert!(
            short_ok.errs.starts_with("Write error, nout=2,"),
            "C :1551 fires on a short write even with asynSuccess, got {:?}",
            short_ok.errs
        );

        // Whole message accepted: NAWT = nwrite and no diagnostic.
        let mut full = writer("test_full_write", 5, false);
        full.process().unwrap();
        assert_eq!(full.nawt, 5);
        assert!(full.errs.is_empty(), "a complete write reports nothing");
    }

    /// C `performIO` raises a record alarm severity for every I/O failure via
    /// `recGblSetSevr` (asynRecord.c:1380-1621): octet/register read error ->
    /// READ/MAJOR, register write error -> WRITE/MAJOR, while the octet *write*
    /// branch (:1551-1555) raises none. The asyn record stages the alarm in
    /// `io_alarm` and `check_alarms` commits it to NSEV/NSTA.
    #[test]
    fn io_errors_raise_record_alarm() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::{AlarmSeverity, CommonFields};
        use tokio::sync::mpsc;

        fn boom() -> AsynError {
            AsynError::Status {
                status: AsynStatus::Timeout,
                message: "boom".into(),
            }
        }

        struct FailDriver(PortDriverBase);
        impl PortDriver for FailDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                Err(boom())
            }
            fn io_write_octet(
                &mut self,
                _user: &mut AsynUser,
                _data: &[u8],
            ) -> crate::error::AsynResult<usize> {
                Err(boom())
            }
            fn io_read_int32(&mut self, _user: &AsynUser) -> crate::error::AsynResult<i32> {
                Err(boom())
            }
            fn io_write_int32(
                &mut self,
                _user: &mut AsynUser,
                _value: i32,
            ) -> crate::error::AsynResult<()> {
                Err(boom())
            }
        }

        let port_name = "test_io_alarm_fail";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(FailDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));

        let mk = |iface: i32, tmod: TransferMode| {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            rec.connect_device();
            rec.iface = iface;
            rec.tmod = tmod as i32;
            rec.imax = 256;
            rec
        };

        // Octet read error -> READ_ALARM / MAJOR (C asynRecord.c:1599).
        let mut octet_rd = mk(0, TransferMode::Read);
        octet_rd.ifmt = ASYN_FMT_ASCII;
        octet_rd.process().unwrap();
        let mut c = CommonFields::default();
        octet_rd.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::READ_ALARM, "octet read err -> READ");
        assert_eq!(c.nsev, AlarmSeverity::Major, "octet read err -> MAJOR");

        // Octet write error raises NO record severity (C :1551-1555 reportError
        // only) — only ERRS.
        let mut octet_wr = mk(0, TransferMode::Write);
        octet_wr.ofmt = ASYN_FMT_ASCII;
        octet_wr.aout = "x".to_string();
        octet_wr.process().unwrap();
        let mut c = CommonFields::default();
        octet_wr.check_alarms(&mut c);
        assert_eq!(
            c.nsev,
            AlarmSeverity::NoAlarm,
            "octet write err raises no record alarm (C parity)"
        );

        // Register read error -> READ_ALARM / MAJOR (C :1393).
        let mut int_rd = mk(1, TransferMode::Read);
        int_rd.process().unwrap();
        let mut c = CommonFields::default();
        int_rd.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::READ_ALARM, "int32 read err -> READ");
        assert_eq!(c.nsev, AlarmSeverity::Major, "int32 read err -> MAJOR");

        // Register write error -> WRITE_ALARM / MAJOR (C :1380).
        let mut int_wr = mk(1, TransferMode::Write);
        int_wr.process().unwrap();
        let mut c = CommonFields::default();
        int_wr.check_alarms(&mut c);
        assert_eq!(
            c.nsta,
            alarm_status::WRITE_ALARM,
            "int32 write err -> WRITE"
        );
        assert_eq!(c.nsev, AlarmSeverity::Major, "int32 write err -> MAJOR");
    }

    /// A port whose driver hands back `min(fill, buf.len())` `Z` bytes with the
    /// given end-of-message reason, and records the buffer size the record asked
    /// for. The requested size IS C's `nread` (asynRecord.c:1512-1517), so it
    /// pins which capacity — `sizeof(ainp)` or IMAX — sized the read.
    fn spawn_fill_port(
        port_name: &'static str,
        fill: usize,
        eom: crate::interpose::EomReason,
    ) -> Arc<Mutex<Option<usize>>> {
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct FillDriver {
            base: PortDriverBase,
            fill: usize,
            eom: EomReason,
            requested: Arc<Mutex<Option<usize>>>,
        }
        impl PortDriver for FillDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                *self.requested.lock().unwrap() = Some(buf.len());
                let n = self.fill.min(buf.len());
                for b in buf[..n].iter_mut() {
                    *b = b'Z';
                }
                Ok((n, self.eom))
            }
        }

        let requested = Arc::new(Mutex::new(None));
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(FillDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                fill,
                eom,
                requested: requested.clone(),
            }),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));
        requested
    }

    fn read_rec(port_name: &str, ifmt: i32, imax: i32, nrrd: i32) -> AsynRecord {
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device();
        rec.iface = 0;
        rec.tmod = TransferMode::Read as i32;
        rec.ifmt = ifmt;
        rec.imax = imax;
        rec.nrrd = nrrd;
        rec
    }

    fn read_alarm(rec: &mut AsynRecord) -> (u16, epics_base_rs::server::record::AlarmSeverity) {
        use epics_base_rs::server::record::CommonFields;
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        (c.nsta, c.nsev)
    }

    /// A port whose driver appends each octet phase it is asked to run to a
    /// shared log, so a test can assert the phase ORDER of one `performIO`
    /// cycle.
    fn spawn_phase_log_port(port_name: &'static str) -> Arc<Mutex<Vec<&'static str>>> {
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct PhaseLogDriver {
            base: PortDriverBase,
            log: Arc<Mutex<Vec<&'static str>>>,
        }
        impl PortDriver for PhaseLogDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> crate::error::AsynResult<()> {
                self.log.lock().unwrap().push("flush");
                Ok(())
            }
            fn io_write_octet(
                &mut self,
                _user: &mut AsynUser,
                data: &[u8],
            ) -> crate::error::AsynResult<usize> {
                self.log.lock().unwrap().push("write");
                Ok(data.len())
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                self.log.lock().unwrap().push("read");
                let resp = b"OK";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                Ok((n, EomReason::EOS))
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(PhaseLogDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                log: log.clone(),
            }),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts);
        register_port(port_name, handle, Arc::new(TraceManager::new()));
        log
    }

    /// R8-47: C flushes the input BEFORE the write, for `TMOD == Flush` *and*
    /// `TMOD == Write_Read` (asynRecord.c:1518-1523) — Write/Read being the
    /// default TMOD. Without it, bytes left in the driver by a previous
    /// transaction are prepended to the fresh response.
    #[test]
    fn write_read_flushes_the_input_before_the_write() {
        let port = "test_tmod_writeread_flush";
        let log = spawn_phase_log_port(port);

        let mut rec = AsynRecord::default();
        rec.port = port.to_string();
        rec.connect_device();
        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::WriteRead as i32;
        rec.ofmt = ASYN_FMT_ASCII;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.aout = "CMD".to_string();
        rec.process().unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["flush", "write", "read"],
            "Write/Read must flush the input before the write"
        );
        assert_eq!(rec.ainp, "OK");
    }

    /// The phase plan is one owner for both runners; pin every TMOD × interface
    /// combination it decides. C: the flush lives inside `performOctetIO`
    /// (asynRecord.c:1518), so a register interface has no flush phase at all —
    /// `performInt32IO` (:1370-1395) only branches on Write / Read.
    #[test]
    fn io_phase_plan_matches_c_perform_io() {
        let plan = |tmod: TransferMode, iface: InterfaceType| -> Vec<IoPhase> {
            let mut rec = AsynRecord::default();
            rec.tmod = tmod as i32;
            rec.iface = iface as i32;
            io_phases(&rec.build_io_plan())
        };
        use InterfaceType::{Int32, Octet};
        use IoPhase::{Flush, Read, Write};

        assert_eq!(plan(TransferMode::WriteRead, Octet), [Flush, Write, Read]);
        assert_eq!(plan(TransferMode::Write, Octet), [Write]);
        assert_eq!(plan(TransferMode::Read, Octet), [Read]);
        assert_eq!(plan(TransferMode::Flush, Octet), [Flush]);
        assert_eq!(plan(TransferMode::NoIo, Octet), []);

        // Register interfaces: no flush phase in C, in any TMOD.
        assert_eq!(plan(TransferMode::WriteRead, Int32), [Write, Read]);
        assert_eq!(plan(TransferMode::Flush, Int32), []);
    }

    /// R8-46: C sizes an ASCII read by `sizeof(pasynRec->ainp)` = 40, NOT by
    /// IMAX (asynRecord.c:1503-1506), and keys the ASCII overflow on the same 40
    /// (`:1602-1608`). With the default IMAX=80 a terminator-less response must
    /// stop at 40 bytes, land 39 chars in AINP, raise READ/MINOR, and leave the
    /// rest of the response in the driver.
    #[test]
    fn ascii_read_is_sized_by_ainp_size_not_imax() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::AlarmSeverity;

        let port = "test_ascii_inlen_is_ainp";
        let requested = spawn_fill_port(port, 100, EomReason::CNT);
        let mut rec = read_rec(port, ASYN_FMT_ASCII, 80, 0);
        rec.process().unwrap();

        assert_eq!(
            *requested.lock().unwrap(),
            Some(AINP_SIZE),
            "the ASCII read length is sizeof(ainp), not IMAX"
        );
        assert_eq!(rec.nord, AINP_SIZE as i32, "NORD is the raw transfer count");
        assert_eq!(rec.ainp.len(), AINP_SIZE - 1, "AINP NUL-truncated at 40");
        assert_eq!(rec.errs, "", "overflow is not an error, only a MINOR alarm");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );
    }

    /// The ASCII overflow boundary is `>=`: 39 bytes fit, 40 overflow.
    #[test]
    fn ascii_read_below_ainp_size_is_not_overflow() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::record::AlarmSeverity;

        let port = "test_ascii_inlen_under";
        spawn_fill_port(port, AINP_SIZE - 1, EomReason::CNT);
        let mut rec = read_rec(port, ASYN_FMT_ASCII, 80, 0);
        rec.process().unwrap();

        assert_eq!(rec.nord, (AINP_SIZE - 1) as i32);
        assert_eq!(rec.ainp.len(), AINP_SIZE - 1, "all 39 bytes land in AINP");
        assert_eq!(read_alarm(&mut rec).1, AlarmSeverity::NoAlarm);
    }

    /// C's overflow test is a plain length compare (`nbytesTransfered >=
    /// sizeof(ainp)`, asynRecord.c:1602) — a full buffer overflows even when the
    /// driver reported an EOS/END end-of-message, so no eomReason may gate it.
    #[test]
    fn ascii_overflow_fires_even_when_the_read_ended_on_eos() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::AlarmSeverity;

        let port = "test_ascii_overflow_eos";
        spawn_fill_port(port, 100, EomReason::EOS);
        let mut rec = read_rec(port, ASYN_FMT_ASCII, 80, 0);
        rec.process().unwrap();

        assert_eq!(rec.nord, AINP_SIZE as i32);
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );
    }

    /// An NRRD-limited read is capped below the buffer capacity, so it can never
    /// reach the overflow threshold (C clamps NRRD to `inlen` and compares the
    /// transfer against `inlen`, asynRecord.c:1513/1602).
    #[test]
    fn nrrd_limited_ascii_read_is_never_overflow() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::record::AlarmSeverity;

        let port = "test_ascii_nrrd_short";
        let requested = spawn_fill_port(port, 100, EomReason::CNT);
        let mut rec = read_rec(port, ASYN_FMT_ASCII, 80, 20);
        rec.process().unwrap();

        assert_eq!(*requested.lock().unwrap(), Some(20), "NRRD sizes the read");
        assert_eq!(rec.nord, 20);
        assert_eq!(rec.ainp.len(), 20, "no truncation below the threshold");
        assert_eq!(read_alarm(&mut rec).1, AlarmSeverity::NoAlarm);
    }

    /// Hybrid is the mode that IS sized by IMAX (it reads into BINP,
    /// asynRecord.c:1507-1510) and whose overflow NULs the buffer's last byte
    /// (`inptr[imax - 1] = '\0'`, :1615).
    #[test]
    fn hybrid_read_is_sized_by_imax_and_nuls_the_buffer_end_on_overflow() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::AlarmSeverity;

        let port = "test_hybrid_inlen_is_imax";
        let requested = spawn_fill_port(port, 100, EomReason::CNT);
        let mut rec = read_rec(port, ASYN_FMT_HYBRID, 4, 0);
        rec.process().unwrap();

        assert_eq!(
            *requested.lock().unwrap(),
            Some(4),
            "IMAX sizes a Hybrid read"
        );
        assert_eq!(rec.nord, 4);
        assert_eq!(rec.binp, vec![b'Z', b'Z', b'Z', 0], "BINP NUL at IMAX-1");
        assert_eq!(rec.ainp, "", "Hybrid must not touch AINP");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );
    }

    /// C `gpibUniversalCmd`/`gpibAddressedCmd` raise COMM/MAJOR when the port
    /// has no asynGpib interface (asynRecord.c:1649/1695); epics-rs ports never
    /// do, so UCMD/ACMD always hits that branch.
    #[test]
    fn gpib_command_raises_comm_alarm() {
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::{AlarmSeverity, CommonFields};

        let mut rec = AsynRecord::default();
        rec.ucmd = 5; // any non-zero GPIB universal command
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynGpib interface");
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM, "no GPIB iface -> COMM");
        assert_eq!(c.nsev, AlarmSeverity::Major, "no GPIB iface -> MAJOR");
    }

    #[test]
    fn test_register_asyn_record_type() {
        register_asyn_record_type();
        let rec = epics_base_rs::server::db_loader::create_record("asyn").unwrap();
        assert_eq!(rec.record_type(), "asyn");
        // Verify it's our full version with all fields
        assert!(rec.field_list().len() > 3);
    }

    /// C parity for `dbTranslateEscape` (epics-base
    /// `libCom/misc/dbTranslateEscape.c`). asynRecord stores OEOS/IEOS
    /// as a backslash-escaped DB field and the device-support layer
    /// MUST decode it before handing off to `pasynOctet->setInputEos`
    /// — otherwise a "\r\n" record string sends four literal bytes
    /// instead of the two-byte terminator.
    #[test]
    fn test_translate_escape_standard_sequences() {
        assert_eq!(translate_escape("\\r\\n"), vec![0x0D, 0x0A]);
        assert_eq!(translate_escape("\\t"), vec![0x09]);
        assert_eq!(translate_escape("\\\\"), vec![b'\\']);
        assert_eq!(translate_escape("\\0"), vec![0x00]);
        assert_eq!(translate_escape("abc"), vec![b'a', b'b', b'c']);
        // Pass-through for unknown escapes (matches C dbTranslateEscape).
        assert_eq!(translate_escape("\\x"), vec![b'\\', b'x']);
        // Dangling backslash passes through.
        assert_eq!(translate_escape("a\\"), vec![b'a', b'\\']);
    }

    #[test]
    fn test_translate_escape_octal() {
        // C dbTranslateEscape decodes octal \N, \NN, \NNN.
        assert_eq!(translate_escape("\\033"), vec![0x1B]); // ESC
        assert_eq!(translate_escape("\\7"), vec![0x07]); // BEL, one digit
        assert_eq!(translate_escape("\\101"), vec![b'A']); // 0o101 == 65
        // Octal escape followed by a non-octal byte stops the run.
        assert_eq!(translate_escape("\\0119"), vec![0x09, b'9']);
        // \0 with no further digits still decodes to NUL.
        assert_eq!(translate_escape("\\0"), vec![0x00]);
        // A terminator built from two octal escapes (e.g. CR LF).
        assert_eq!(translate_escape("\\015\\012"), vec![0x0D, 0x0A]);
    }

    #[test]
    fn test_octet_output_buffer_by_ofmt() {
        let mut rec = AsynRecord::default();

        // ASCII: AOUT is escape-translated, full buffer (no NOWT clamp).
        rec.ofmt = ASYN_FMT_ASCII;
        rec.aout = "hi\\r\\n".to_string();
        rec.nowt = 2; // would have truncated under the old all-mode clamp
        assert_eq!(
            rec.octet_output_buffer(),
            vec![b'h', b'i', 0x0D, 0x0A],
            "ASCII must escape-translate AOUT and ignore NOWT"
        );

        // Hybrid: BOUT (as a C string) is escape-translated.
        rec.ofmt = ASYN_FMT_HYBRID;
        rec.bout = b"x\\t".to_vec();
        assert_eq!(
            rec.octet_output_buffer(),
            vec![b'x', 0x09],
            "Hybrid must escape-translate the BOUT buffer"
        );
        // Hybrid stops at an interior NUL (C-string semantics).
        rec.bout = b"ab\0cd".to_vec();
        assert_eq!(rec.octet_output_buffer(), vec![b'a', b'b']);

        // Binary: raw BOUT, no translation, NOWT bytes clamped to OMAX.
        rec.ofmt = ASYN_FMT_BINARY;
        rec.bout = vec![b'\\', b'r', 0x00, 0x01, 0x02];
        rec.omax = 80;
        rec.nowt = 4;
        assert_eq!(
            rec.octet_output_buffer(),
            vec![b'\\', b'r', 0x00, 0x01],
            "Binary writes raw BOUT untranslated, NOWT bytes"
        );
        // NOWT clamps to OMAX.
        rec.omax = 3;
        rec.nowt = 10;
        assert_eq!(rec.octet_output_buffer(), vec![b'\\', b'r', 0x00]);
    }

    #[test]
    fn test_tfil_special_targets() {
        // C asynRecord.c:453-461 bracketed-token convention. Empty maps to
        // stdout (NOT stderr), and only the bracketed names are special.
        assert!(matches!(open_trace_file("").unwrap(), TraceFile::Stdout));
        assert!(matches!(
            open_trace_file("<stdout>").unwrap(),
            TraceFile::Stdout
        ));
        assert!(matches!(
            open_trace_file("<stderr>").unwrap(),
            TraceFile::Stderr
        ));
        assert!(matches!(
            open_trace_file("<errlog>").unwrap(),
            TraceFile::Errlog
        ));
    }

    #[test]
    fn test_tfil_bare_names_are_file_paths() {
        // C treats a bare "stdout"/"stderr" as a literal filename (only the
        // bracketed tokens are special). Open them in a temp dir so the
        // resolved sink is a File, not a console.
        let dir = std::env::temp_dir().join(format!("asynrec_tfil_bare_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["stdout", "stderr"] {
            let path = dir.join(name);
            let p = path.to_str().unwrap();
            assert!(
                matches!(open_trace_file(p).unwrap(), TraceFile::File(_)),
                "bare {name} must resolve to a file path, not a console sink"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tfil_path_appends_not_truncates() {
        // C opens trace files with fopen(.., "a+"); a second open must keep
        // earlier content rather than truncating it (File::create did).
        let path = std::env::temp_dir().join(format!("asynrec_tfil_append_{}", std::process::id()));
        let p = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        open_trace_file(p).unwrap().write_line("first\n");
        // Re-open (as a record re-applying TFIL would) and append more.
        open_trace_file(p).unwrap().write_line("second\n");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents, "first\nsecond\n",
            "re-opening a trace file must append, not truncate"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Minimal `can_block` port whose Int32 parameter 0 holds a known value,
    /// backed by a real [`PortActor`] thread — the off-thread orchestration
    /// submits against it exactly as a production blocking driver.
    fn canblock_int32_entry(value: i32) -> super::registry::PortEntry {
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use tokio::sync::mpsc;

        struct ReadDriver {
            base: PortDriverBase,
        }
        impl PortDriver for ReadDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new("ASYNIO", 1, PortFlags::default());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        base.set_int32_param(val, 0, value).unwrap();

        let (tx, rx) = mpsc::channel(16);
        let actor = PortActor::new(Box::new(ReadDriver { base }), rx);
        std::thread::Builder::new()
            .name("asynio-test-actor".into())
            .spawn(move || actor.run())
            .unwrap();

        let mut handle = PortHandle::new(tx, "ASYNIO".into(), Arc::new(InterruptManager::new(16)));
        handle.set_can_block(true);
        super::registry::PortEntry {
            handle,
            trace: Arc::new(TraceManager::new()),
        }
    }

    /// Boundary: a `can_block` port with a live async context defers `performIO`
    /// off the scan thread (C `process()` `canBlock` branch, asynRecord.c:344-350)
    /// — `process()` returns `AsyncPending` without the I/O result, and the
    /// completion re-entry (C `pact==TRUE`, asynRecord.c:362-363) applies it.
    #[tokio::test]
    async fn nonblocking_canblock_port_defers_then_applies_on_reentry() {
        use epics_base_rs::server::database::PvDatabase;

        let mut rec = AsynRecord::default();
        rec.port_entry = Some(canblock_int32_entry(7));
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        rec.resolved_reason = 0;

        // A live database handle whose record set does NOT contain this record:
        // the orchestration's re-entry token resolves to nothing, so the test
        // drives the completion re-entry itself, isolating the apply path.
        let db = PvDatabase::new();
        rec.async_ctx = Some(("ASYNIO_REC".to_string(), db.async_handle()));

        // Pass 1: submitted off-thread, parked — no inline I/O result.
        let out = rec.process().unwrap();
        assert_eq!(
            out.result,
            RecordProcessResult::AsyncPending,
            "a can_block port with async context must defer, not run inline"
        );
        assert!(
            rec.io_inflight.is_some(),
            "the deferred request is held in io_inflight until completion"
        );
        assert_eq!(
            rec.i32inp, 0,
            "the scan thread returned before the read value landed"
        );

        // The off-thread orchestration fills the shared result slot.
        let slot = rec.io_inflight.as_ref().unwrap().result.clone();
        let mut filled = false;
        for _ in 0..2000 {
            if slot.lock().unwrap().is_some() {
                filled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(filled, "the orchestration must fill the result slot");

        // Pass 2: completion re-entry applies the read value and finishes.
        let out2 = rec.process().unwrap();
        assert_eq!(out2.result, RecordProcessResult::Complete);
        assert!(
            rec.io_inflight.is_none(),
            "completion re-entry clears the in-flight slot"
        );
        assert_eq!(rec.i32inp, 7, "the read value is applied on re-entry");
    }

    /// Boundary: `AQR` (Abort Queue Request) that loses the race to the port
    /// thread is the C `wasQueued==0` / `callbackActive` case. C `special()`
    /// for `AQR` (asynRecord.c:393-408) calls `cancelRequest`; once the request
    /// has been dequeued and its callback is running, `cancelRequest` reports
    /// `wasQueued==0` and waits for the callback (asynManager.c:1645-1659), so
    /// the I/O runs to completion and is reported normally — `AQR` does NOT
    /// raise "I/O request canceled". Here the read is already parked in the
    /// driver (the executor has claimed the token, `Running`) when `AQR` fires,
    /// so the completion re-entry applies the device value, not CANCELED.
    #[tokio::test]
    async fn aqr_after_driver_dequeue_runs_to_completion() {
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use std::sync::Barrier;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::mpsc;

        // A can_block port whose Int32 read parks on a barrier until released,
        // signalling when it has entered the driver so the test can cancel
        // mid-flight.
        struct BlockingReadDriver {
            base: PortDriverBase,
            value: i32,
            entered: Arc<AtomicBool>,
            release: Arc<Barrier>,
        }
        impl PortDriver for BlockingReadDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
                self.entered.store(true, Ordering::SeqCst);
                self.release.wait();
                Ok(self.value)
            }
        }

        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Barrier::new(2));
        let mut base = PortDriverBase::new("ASYNAQR", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let driver = BlockingReadDriver {
            base,
            value: 7,
            entered: entered.clone(),
            release: release.clone(),
        };

        let (tx, rx) = mpsc::channel(16);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("asynaqr-test-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let mut handle = PortHandle::new(tx, "ASYNAQR".into(), Arc::new(InterruptManager::new(16)));
        handle.set_can_block(true);
        let entry = super::registry::PortEntry {
            handle,
            trace: Arc::new(TraceManager::new()),
        };

        let mut rec = AsynRecord::default();
        rec.port_entry = Some(entry);
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        rec.resolved_reason = 0;
        let db = PvDatabase::new();
        rec.async_ctx = Some(("ASYNAQR_REC".to_string(), db.async_handle()));

        // No request in flight yet: AQR is the C `wasQueued==false` no-op.
        rec.special("AQR", true).unwrap();
        assert!(rec.errs.is_empty(), "AQR with no in-flight request is idle");

        // Submit off-thread, then wait until the read is parked in the driver.
        let out = rec.process().unwrap();
        assert_eq!(out.result, RecordProcessResult::AsyncPending);
        for _ in 0..2000 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "the off-thread read must reach the driver before AQR"
        );

        // AQR fires while the read is already running in the driver: the token
        // is `Running`, so the cancel loses (C `wasQueued==0`). Release the
        // parked read so the phase completes and the orchestration finishes.
        rec.special("AQR", true).unwrap();
        release.wait();

        let slot = rec.io_inflight.as_ref().unwrap().result.clone();
        let mut filled = false;
        for _ in 0..2000 {
            if slot.lock().unwrap().is_some() {
                filled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(filled, "the running request still produces an outcome");

        // Completion re-entry applies the device value: the cancel lost the
        // race (C `wasQueued==0`), so the I/O ran to completion and is reported
        // normally — no "I/O request canceled".
        let out2 = rec.process().unwrap();
        assert_eq!(out2.result, RecordProcessResult::Complete);
        assert!(rec.io_inflight.is_none(), "completion re-entry leaves idle");
        assert!(
            rec.errs.is_empty(),
            "a cancel that lost the race does not report CANCELED (wasQueued==0)"
        );
        assert_eq!(
            rec.i32inp, 7,
            "the device read value applies normally when the cancel loses the race"
        );
    }

    /// Boundary: an external `setTraceMask` posts the trace readback fields
    /// immediately, out of band, with no intervening `process()`. C
    /// `exceptCallback` → `monitorStatus` re-posts the changed trace fields
    /// under `dbScanLock` (asynRecord.c:903-917,1102-1117); the Rust callback
    /// `post_fields`-es them through the database handle. The mask change is
    /// driven from a non-runtime thread (the iocsh / port-actor case) to
    /// exercise the captured runtime handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trace_change_posts_readback_fields_immediately() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use tokio::sync::mpsc;

        struct TraceDriver(PortDriverBase);
        impl PortDriver for TraceDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_trace_immediate_post";
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(TraceDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), Arc::new(InterruptManager::new(256)));

        // A trace manager with an exception sink so trace changes announce
        // (without it, `exception_manager()` is None and no callback registers).
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Known baseline mask, then register the port for the record to find.
        trace.set_trace_mask(Some(port_name), TraceMask::empty());
        super::registry::register_port(port_name, handle, trace.clone());

        // Build the record, hand it the database handle, and connect it —
        // connecting registers the trace exception callback with the same
        // async_ctx + runtime handle the framework supplies at add_record.
        let db = PvDatabase::new();
        let rec_name = "TRACE_IMM_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.to_string(), db.async_handle());
        rec.connect_device();
        assert_eq!(rec.cnct, 1, "record must connect to the registered port");
        assert_eq!(rec.tmsk, 0, "baseline trace mask is empty");

        db.add_record(rec_name, Box::new(rec)).await.unwrap();

        // Externally change the trace mask from a non-runtime thread. The
        // captured runtime handle must drive the out-of-band post.
        let new_mask = TraceMask::ERROR | TraceMask::FLOW;
        {
            let tm = trace.clone();
            let pn = port_name.to_string();
            std::thread::spawn(move || tm.set_trace_mask(Some(&pn), new_mask))
                .join()
                .unwrap();
        }

        // TMSK/TB0/TB4 reflect the new mask with no intervening process().
        let want = new_mask.bits() as i32;
        let mut posted = false;
        for _ in 0..2000 {
            let inst = db.get_record(rec_name).await.unwrap();
            let got = inst.read().await.record.get_field("TMSK");
            if got == Some(EpicsValue::Long(want)) {
                posted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            posted,
            "trace change must post TMSK immediately, no process()"
        );

        let inst = db.get_record(rec_name).await.unwrap();
        let g = inst.read().await;
        assert_eq!(g.record.get_field("TMSK"), Some(EpicsValue::Long(want)));
        assert_eq!(
            g.record.get_field("TB0"),
            Some(EpicsValue::Short(1)),
            "ERROR bit posted"
        );
        assert_eq!(
            g.record.get_field("TB4"),
            Some(EpicsValue::Short(1)),
            "FLOW bit posted"
        );
        assert_eq!(
            g.record.get_field("TB1"),
            Some(EpicsValue::Short(0)),
            "IO_DEVICE bit stays clear"
        );
    }

    /// Boundary: the out-of-band trace post mirrors C `POST_IF_NEW`
    /// (asynRecord.c:210-214,1102-1117) — `monitorStatus` posts a readback
    /// field only when its value differs from the remembered value. An
    /// `asynSetTraceIOMask` exception recomputes ALL trace readback fields, but
    /// only the IO fields changed, so the unchanged `TMSK` must NOT be
    /// re-posted to its monitor (the base `post_fields` path posts
    /// unconditionally, so the dedup lives in the record's last-posted cache).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_trace_field_is_not_reposted() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::{TraceIoMask, TraceManager};
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::db_access::DbSubscription;
        use std::time::Duration;
        use tokio::sync::mpsc;

        struct TraceDriver(PortDriverBase);
        impl PortDriver for TraceDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_trace_post_if_new";
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(TraceDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), Arc::new(InterruptManager::new(256)));

        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Baseline masks BEFORE connect so the callback seeds its last-posted
        // cache with these values (the C `old` after the connect-path
        // `monitorStatus`): TMSK = ERROR, all IO bits clear.
        trace.set_trace_mask(Some(port_name), TraceMask::ERROR);
        trace.set_trace_io_mask(Some(port_name), TraceIoMask::empty());
        super::registry::register_port(port_name, handle, trace.clone());

        let db = PvDatabase::new();
        let rec_name = "TRACE_POSTIFNEW_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.to_string(), db.async_handle());
        rec.connect_device();
        assert_eq!(rec.cnct, 1, "record must connect to the registered port");
        db.add_record(rec_name, Box::new(rec)).await.unwrap();

        let mut tmsk_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.TMSK"))
            .await
            .expect("subscribe TMSK");
        let mut tiom_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.TIOM"))
            .await
            .expect("subscribe TIOM");

        // Positive control: a trace-mask change DOES post the changed TMSK.
        let new_tmsk = TraceMask::ERROR | TraceMask::FLOW;
        {
            let (tm, pn) = (trace.clone(), port_name.to_string());
            std::thread::spawn(move || tm.set_trace_mask(Some(&pn), new_tmsk))
                .join()
                .unwrap();
        }
        assert_eq!(
            tmsk_sub.recv().await,
            Some(EpicsValue::Long(new_tmsk.bits() as i32)),
            "a changed TMSK is posted to its monitor"
        );

        // The dedup case: change ONLY the IO mask. The callback recomputes all
        // trace fields but TMSK is unchanged, so only the IO fields post.
        {
            let (tm, pn) = (trace.clone(), port_name.to_string());
            std::thread::spawn(move || tm.set_trace_io_mask(Some(&pn), TraceIoMask::ASCII))
                .join()
                .unwrap();
        }
        // The changed IO field IS posted — this also synchronises the post
        // task. `trace_readback_fields` orders TMSK (index 0) before TIOM
        // (index 7) in the same `post_fields` call, so once the TIOM event
        // arrives a non-deduped TMSK duplicate would already be queued.
        assert_eq!(
            tiom_sub.recv().await,
            Some(EpicsValue::Long(TraceIoMask::ASCII.bits() as i32)),
            "a changed TIOM is posted to its monitor"
        );
        // The unchanged TMSK must NOT have been re-posted.
        let reposted = tokio::time::timeout(Duration::from_millis(500), tmsk_sub.recv()).await;
        assert!(
            reposted.is_err(),
            "unchanged TMSK must not be re-posted on an IO-mask-only change, got {reposted:?}"
        );
    }
}
