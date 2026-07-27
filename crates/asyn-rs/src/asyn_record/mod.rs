pub mod device;
mod io_intr;
pub use device::{ASYN_RECORD_DTYP, AsynRecordDevice};
// The port registry itself is [`crate::registry`], not this module's: it is the
// framework's process-wide name claim, used by `crate::manager` whether or not
// an EPICS record layer exists. Re-exported here because every existing caller
// — `ad-core-rs`, `ad-plugins-rs`, `modbus-rs`, `mqtt-rs` and the example IOCs
// — spells it `asyn_record::get_port`, and moving the definition is not a
// reason to break them.
pub use crate::registry::{
    PortEntry, PortRegistry, get_port, port_names, register_port, unregister_port,
};

use io_intr::{IoIntrBinding, IoIntrSample, IoIntrScan};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, FieldDeclaration, ProcessOutcome, Record, RecordProcessResult,
    ScanType,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::{AsynException, ExceptionCallbackId, ExceptionManager};
use crate::interpose::EomReason;
use crate::port_handle::PortHandle;
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use crate::user::AsynUser;

/// Return the asyn record type factory for injection into IocBuilder.
///
/// This and [`register_asyn_record_type`] are what actually belonged to this
/// module when the port registry was split out to [`crate::registry`]: both
/// name `epics_base_rs` record machinery, so both exist only under the `epics`
/// feature, and neither is reachable from the framework core.
pub fn asyn_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("asyn", Box::new(|| Box::new(AsynRecord::default())))
}

/// Register the "asyn" record type via the global registry (legacy).
/// Prefer `asyn_record_factory()` with `IocBuilder::register_record_type()`.
pub fn register_asyn_record_type() {
    epics_base_rs::server::db_loader::register_record_type(
        "asyn",
        Box::new(|| Box::new(AsynRecord::default())),
    );
}

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
pub(crate) enum InterfaceType {
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

    /// The asyn interface this IFACE selection performs I/O through — what the
    /// driver must create the parameter as when it resolves this record's
    /// drvInfo on demand.
    fn as_asyn_iface(self) -> crate::interfaces::InterfaceType {
        match self {
            Self::Octet => crate::interfaces::InterfaceType::Octet,
            Self::Int32 => crate::interfaces::InterfaceType::Int32,
            Self::UInt32Digital => crate::interfaces::InterfaceType::UInt32Digital,
            Self::Float64 => crate::interfaces::InterfaceType::Float64,
        }
    }

    /// The interface's name as C splices it into the register-handler ERRS
    /// diagnostics — `"Int32 write error, %s"` (asynRecord.c:1378) and its five
    /// siblings at :1391/:1414/:1429/:1450/:1463. C writes `UInt32`, not
    /// `UInt32Digital`.
    ///
    /// The octet handlers have their own formats (`"Write error, nout=%d, %s"`,
    /// `"%s  nread %d %s"`, `"Overflow nread %d %s"`) and never reach this, so
    /// the `Octet` arm exists only to keep the match total.
    fn c_errs_name(self) -> &'static str {
        match self {
            Self::Octet => "Octet",
            Self::Int32 => "Int32",
            Self::UInt32Digital => "UInt32",
            Self::Float64 => "Float64",
        }
    }

    /// The asyn interface name C reports when the port does not implement it —
    /// `"No asynUInt32Digital interface"` (asynRecord.c:1345), spelled in full,
    /// unlike [`Self::c_errs_name`].
    fn c_asyn_name(self) -> &'static str {
        match self {
            Self::Octet => "asynOctet",
            Self::Int32 => "asynInt32",
            Self::UInt32Digital => "asynUInt32Digital",
            Self::Float64 => "asynFloat64",
        }
    }

    /// The `crate::interfaces` interface this IFACE selects — the record's link
    /// to the port's interface registry.
    fn registry_type(self) -> crate::interfaces::InterfaceType {
        match self {
            Self::Octet => crate::interfaces::InterfaceType::Octet,
            Self::Int32 => crate::interfaces::InterfaceType::Int32,
            Self::UInt32Digital => crate::interfaces::InterfaceType::UInt32Digital,
            Self::Float64 => crate::interfaces::InterfaceType::Float64,
        }
    }
}

// ===== Option menu choice tables =====
//
// C `setOption` indexes these arrays with the record's menu field and hands the
// text straight to `pasynOption->setOption` — `baud_choices[pasynRec->baud]`,
// `parity_choices[pasynRec->prty]`, … (asynRecord.c:49-66, :1777-1826). Index 0
// of every menu is the literal "Unknown", and C sends it like any other choice:
// a put of `Unknown` is a real `setOption("baud", "Unknown")` the driver
// rejects, reported as "Error setting option, …", after which the `/* no break */`
// fall-through into `getOptions` (asynRecord.c:845-849) refreshes *every* option
// readback and snaps the field back to the driver's actual value. It is not a
// silent no-op, and a record must not treat it as one — that is what leaves an
// operator's mis-set field showing a value the port never took.

/// C `baud_choices` (asynRecord.c:49-53).
const BAUD_CHOICES: &[&str] = &[
    "Unknown", "300", "600", "1200", "2400", "4800", "9600", "19200", "38400", "57600", "115200",
    "230400", "460800", "576000", "921600", "1152000",
];
/// C `parity_choices` (asynRecord.c:54).
const PARITY_CHOICES: &[&str] = &["Unknown", "none", "even", "odd"];
/// C `data_bit_choices` (asynRecord.c:56).
const DBIT_CHOICES: &[&str] = &["Unknown", "5", "6", "7", "8"];
/// C `stop_bit_choices` (asynRecord.c:58).
const SBIT_CHOICES: &[&str] = &["Unknown", "1", "2"];
/// C `modem_control_choices` (asynRecord.c:60).
const MCTL_CHOICES: &[&str] = &["Unknown", "Y", "N"];
/// C `flow_control_choices` (asynRecord.c:62).
const FCTL_CHOICES: &[&str] = &["Unknown", "N", "Y"];
/// C `ix_control_choices` (asynRecord.c:64) — IXON, IXOFF and IXANY share it.
const IX_CHOICES: &[&str] = &["Unknown", "N", "Y"];
/// C `drto_choices` (asynRecord.c:66).
const DRTO_CHOICES: &[&str] = &["Unknown", "N", "Y"];

/// C `<menu>_choices[index]` — the record's menu index into the choice text it
/// sends to the driver.
///
/// C indexes the array unchecked because a dbd menu field cannot hold an index
/// outside its menu (dbPut validates the choice against the menu). This port's
/// fields are plain `i32`, so an index C could never produce takes the menu's
/// index-0 "Unknown" — the choice that configures nothing and that the driver
/// rejects, which is the closest defined behavior to C's.
fn menu_choice(choices: &'static [&'static str], index: i32) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|i| choices.get(i).copied())
        .unwrap_or(choices[0])
}

/// The `asynBAUD` menu index for the text the driver reported — C `getOptions`'
/// `for (i…) if (strcmp(optbuff, baud_choices[i]) == 0) pasynRec->baud = i;`
/// (asynRecord.c:1868-1871), which walks the very array `setOption` writes from.
/// Text that matches no choice reads back as index 0, "Unknown", exactly as C's
/// `pasynRec->baud = 0` before the loop leaves it.
///
/// Derived from [`BAUD_CHOICES`] so the record cannot send one text and read back
/// against another.
fn baud_choice_index(text: &str) -> i32 {
    BAUD_CHOICES
        .iter()
        .position(|choice| *choice == text)
        .unwrap_or(0) as i32
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

/// The record's two `SPC_DBADDR` byte-array fields — the ones C's `cvt_dbaddr`
/// hands `dbPut` a buffer for, and therefore the ones whose element count
/// `put_array_info` records (asynRecord.c:940-993). Naming them keeps
/// [`AsynRecord::put_array_field`] — the single writer of buffer *and* count —
/// exhaustive: a third array field cannot be added without deciding which count
/// it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsynArrayField {
    /// BOUT, whose count is NOWT.
    Bout,
    /// BINP, whose count is NORD.
    Binp,
}

/// Capacity of the `AINP` input string, in bytes — `sizeof(pasynRec->ainp)`.
/// `AINP` is a `DBF_STRING` (`asynRecord.dbd:261`), i.e. EPICS
/// `MAX_STRING_SIZE` = 40 including the NUL terminator. C `performOctetIO`
/// sizes the ASCII read by exactly this (`asynRecord.c:1505-1506`
/// `inlen = sizeof(pasynRec->ainp)`) and keys the ASCII overflow on it
/// (`:1602-1608`), independent of `IMAX` — only Hybrid and Binary read into the
/// `IMAX`-sized `BINP` buffer.
const AINP_SIZE: usize = 40;

/// Capacity of the `TINP` translated-input string, in bytes —
/// `sizeof(pasynRec->tinp)`, a `DBF_STRING` like `AINP`. C passes exactly this
/// as the `epicsStrSnPrintEscaped` destination length on both TINP writers
/// (`asynRecord.c:725` I/O-Intr, `:1629` polled read), so an escaped form longer
/// than 39 characters is cut there — mid escape pair if that is where the count
/// runs out.
pub(super) const TINP_SIZE: usize = 40;

/// C `EOS_SIZE` (asynRecord.c:68) — the size of the `inputEosTranslate` /
/// `outputEosTranslate` buffers `getEos` escapes the driver's terminators into
/// (`:1990-1991`, `:2005`, `:2012`) before posting them to IEOS/OEOS. A driver
/// EOS whose escaped form exceeds 9 characters reaches the record truncated.
pub(super) const EOS_SIZE: usize = 10;

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

    // Set by the exception callback (C `exceptCallback`, asynRecord.c:903-917)
    // when ANY asyn exception fires on the record's port — an external
    // `setTrace{Mask,IOMask,InfoMask}`, a connect / disconnect, an enable or
    // auto-connect change; consumed by `process()`, which re-runs the whole
    // C `monitorStatus` refresh through [`AsynRecord::monitor_status`]. This is
    // the fallback for a record with no database handle or no runtime: with
    // both, the callback refreshes and posts immediately, out of band, as C
    // does — it never sets this flag.
    status_dirty: Arc<AtomicBool>,
    // Owner handle for the registered exception callback so it can
    // be removed on disconnect / drop (C `exceptionCallbackRemove`,
    // asynRecord.c:523,1154,1313).
    except_cb: Option<(Arc<ExceptionManager>, ExceptionCallbackId)>,
    // The record alarm `(stat, sevr)` raised by this process cycle's I/O —
    // the C `recGblSetSevr(pasynRec, …)` calls in `performIO`
    // (asynRecord.c:1380-1621) and the no-asynGpib-interface COMM alarm
    // (:1649/:1695). Set by `apply_io_outcome` / the UCMD-ACMD short-circuits;
    // the single consumer is `check_alarms`, which `take()`s it and commits it
    // via `rec_gbl_set_sevr`. Every complete-returning process cycle reaches
    // `check_alarms`, so the `take()` is the single clear point — no path
    // stages an alarm and then bypasses the consumer.
    io_alarm: Option<(u16, AlarmSeverity)>,
    // C `old.traceFd` (asynRecord.c:203): the identity of the trace sink this
    // record last saw or installed. `monitorStatus` compares the port's current
    // sink against it and writes TFIL = "Unknown" when another thread re-pointed
    // the trace file (:1119-1124). Shared with the out-of-band `exceptCallback`
    // refresh, which runs the same comparison — one cell, so the two paths
    // cannot disagree about which sink is "ours". `None` until the first sample.
    old_trace_file_id: Arc<Mutex<Option<usize>>>,

    // C `asynRecPvt`'s I/O Intr half — `ioScanPvt`, `interruptPvt`,
    // `interruptLock` and `gotValue` (asynRecord.c:228-232). Shared with the
    // record's `asynRecordDevice` device support, which is the framework's
    // `get_ioint_info` seam; see [`io_intr`].
    io_intr: Arc<IoIntrScan>,
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
            // C `asynRecord.c` init_record pass 0: `strcpy(prec->tfil,
            // "Unknown")` — set unconditionally, no port needed. The `.dbd`
            // carries no `initial(...)` for TFIL, so the seed lives in record
            // code; served before any client read. (Same fix as epics-base-rs's
            // display stub; this is the record Path 1 actually serves.)
            tfil: TFIL_UNKNOWN.to_string(),
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
            status_dirty: Arc::new(AtomicBool::new(false)),
            except_cb: None,
            io_alarm: None,
            old_trace_file_id: Arc::new(Mutex::new(None)),
            io_intr: Arc::new(IoIntrScan::new()),
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

/// The GPIB command a cycle carries, decoded from UCMD / ACMD at the top of the
/// cycle — C `asynCallbackProcess` dispatches `gpibUniversalCmd` (UCMD) or
/// `gpibAddressedCmd` (ACMD) *instead of* `performIO` (asynRecord.c:819-827), so
/// a cycle is either a GPIB command or a transfer, never both.
#[derive(Clone, PartialEq, Eq, Debug)]
enum GpibCycle {
    /// One `universalCmd(cmd)` — C `gpibUniversalCmd` (asynRecord.c:1638-1677).
    Universal(u8),
    /// One `addressedCmd(frame)` — C `gpibAddressedCmd`'s `acmd[]`
    /// (asynRecord.c:1698-1750).
    Addressed(Vec<u8>),
    /// C's three-operation Serial Poll (asynRecord.c:1717-1746).
    SerialPoll,
}

/// Immutable snapshot of the record fields `performIO` reads, built on the
/// scan thread so the off-thread orchestration never touches the record.
struct IoPlan {
    tmod: TransferMode,
    iface: InterfaceType,
    /// `Some` when this cycle is a GPIB command rather than a transfer — the
    /// UCMD / ACMD the operator put, decoded and consumed by
    /// [`AsynRecord::take_gpib_cycle`].
    gpib: Option<GpibCycle>,
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
    /// The serial-poll response byte C reads straight into the SPR field
    /// (`read(…, (char *) &pasynRec->spr, 1, …)`, asynRecord.c:1729-1731).
    spr: Option<i32>,
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

impl IoOutcome {
    /// C `reportError(pasynRec, status, format, ...)` (asynRecord.c:2028-2049)
    /// — the single formatter every `performIO` diagnostic passes through, and
    /// the only writer of `ERRS` on the I/O path.
    ///
    /// C `strncpy`s the formatted text *over* `ERRS`, so within one cycle the
    /// **last** `reportError` wins; the recorders below therefore call this in
    /// C's own source order (read status before overflow, asynRecord.c:1593-1615)
    /// rather than in whatever order reads more naturally.
    ///
    /// Every C `reportError` format ends in `%s` = `pasynUser->errorMessage`,
    /// the driver's own diagnostic. [`AsynError::message`] is its port analogue
    /// — it reads *through* the `PartialRead`/`PartialWrite` carriers to the
    /// underlying driver text — so it, not `Display`, is what belongs in that
    /// tail.
    fn report_error(&mut self, msg: String) {
        self.errs = Some(msg);
    }

    /// The AQR-cancel outcome, C `special()`'s `wasQueued` branch
    /// (asynRecord.c:397-400): `reportError(… "I/O request canceled")` **and**
    /// `recGblSetSevr(pasynRec, STATE_ALARM, MAJOR_ALARM)`, then the forced
    /// completion callback. The message and the severity are one event in C and
    /// have one owner here, so a cancel cannot land in `ERRS` with the record
    /// left in NO_ALARM. The completion re-entry that applies this outcome
    /// (`apply_io_outcome` → `check_alarms`) is C's forced callback.
    fn report_canceled(&mut self) {
        self.report_error(CANCELED_MSG.to_string());
        raise_io_alarm(self, alarm_status::STATE_ALARM, AlarmSeverity::Major);
    }

    /// The queue-timeout outcome of a **process** request, C
    /// `queueTimeoutCallbackProcess` (asynRecord.c:919-926): `reportError(…
    /// "process queueRequest timeout")`, `recGblSetSevr(STATE_ALARM,
    /// MAJOR_ALARM)`, then `callbackRequestProcessCallback` to complete the
    /// record that is still sitting at `pact = TRUE`.
    ///
    /// The same shape as [`Self::report_canceled`], and for the same reason: both
    /// are "the queued request left the queue without running", so the message
    /// and the severity are one event with one owner, and the completion re-entry
    /// that applies this outcome is C's forced callback. The remaining phases of
    /// the plan do **not** run — C never entered `performIO` at all.
    fn report_queue_timeout(&mut self) {
        self.report_error(PROCESS_QUEUE_TIMEOUT_MSG.to_string());
        raise_io_alarm(self, alarm_status::STATE_ALARM, AlarmSeverity::Major);
    }

    /// The **refused** process request, C `process()` (asynRecord.c:342-361): the
    /// queue gate turned `queueRequest` down (`asynDisabled` / `asynDisconnected`,
    /// asynManager.c:1541-1552), so `performIO` never ran. C reports the literal
    /// `"queueRequest failed"` and alarms `STATE_ALARM`/`MINOR_ALARM` — the same
    /// arm that answers a record with no port ("Not connect to a port", :356-357)
    /// — and posts no transfer field, because there was no transfer.
    ///
    /// The process-side twin of [`AsynRecord::report_special_never_ran`]: a
    /// refusal is `queueRequest`'s *return value*, not a driver diagnostic raised
    /// inside a callback that ran, so it must not be dressed up as one (R14-46).
    /// Without this the port reported a disabled port as `"Read error, port X is
    /// disabled"` with a COMM/MAJOR alarm and published NORD/EOMR for a read that
    /// never reached the driver.
    fn report_queue_refused(&mut self) {
        self.report_error(PROCESS_QUEUE_REFUSED_MSG.to_string());
        raise_io_alarm(self, alarm_status::STATE_ALARM, AlarmSeverity::Minor);
    }
}

/// The status word C splices into the final octet read-error message
/// (asynRecord.c:1594-1596): `asynTimeout` -> `timeout`, `asynOverflow` ->
/// `overflow`, everything else -> `error`.
fn c_read_status_word(e: &crate::error::AsynError) -> &'static str {
    use crate::error::AsynStatus;
    match e.status() {
        AsynStatus::Timeout => "timeout",
        AsynStatus::Overflow => "overflow",
        _ => "error",
    }
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
        .with_queue_timeout(QUEUE_TIMEOUT)
}

/// Build the `asynUser` for a `Flush` phase. C `asynRecord.c` issues the flush
/// with the same reason/addr but no transfer timeout.
fn flush_user(plan: &IoPlan) -> AsynUser {
    AsynUser::new(plan.reason)
        .with_addr(plan.addr)
        .with_queue_timeout(QUEUE_TIMEOUT)
}

/// One phase of a record I/O cycle — a `performIO` transfer phase, or one bus
/// operation of a GPIB command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IoPhase {
    Flush,
    Write,
    Read,
    /// C `gpibUniversalCmd`'s single `universalCmd(cmd_char)`
    /// (asynRecord.c:1672).
    GpibUniversal(u8),
    /// C `gpibAddressedCmd`'s `addressedCmd(acmd, lenCmd)` (asynRecord.c:1750);
    /// the frame is [`GpibCycle::Addressed`]'s.
    GpibAddressed,
    /// Serial Poll step 1 — `universalCmd(IBSPE)` (asynRecord.c:1719-1726).
    GpibSerialPollEnable,
    /// Serial Poll step 2 — a one-byte octet read into SPR
    /// (asynRecord.c:1728-1735).
    GpibSerialPollRead,
    /// Serial Poll step 3 — `universalCmd(IBSPD)` (asynRecord.c:1737-1744).
    /// C runs it even when the enable or the read failed, so it is a phase like
    /// any other: a failing phase reports, it does not abort the cycle.
    GpibSerialPollDisable,
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
    // A GPIB command replaces the transfer entirely — C dispatches
    // gpibUniversalCmd / gpibAddressedCmd *instead of* performIO
    // (asynRecord.c:819-827), so TMOD and IFACE do not apply to this cycle.
    if let Some(cycle) = &plan.gpib {
        return match cycle {
            GpibCycle::Universal(cmd) => vec![IoPhase::GpibUniversal(*cmd)],
            GpibCycle::Addressed(_) => vec![IoPhase::GpibAddressed],
            GpibCycle::SerialPoll => vec![
                IoPhase::GpibSerialPollEnable,
                IoPhase::GpibSerialPollRead,
                IoPhase::GpibSerialPollDisable,
            ],
        };
    }
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

/// The port operation one phase submits — the single owner of phase→`RequestOp`
/// for both the synchronous [`AsynRecord::perform_io`] and the off-thread
/// [`run_io_plan`].
fn io_phase_op(plan: &IoPlan, phase: IoPhase) -> RequestOp {
    match phase {
        IoPhase::Flush => RequestOp::Flush,
        IoPhase::Write => io_write_op(plan),
        IoPhase::Read => io_read_op(plan),
        IoPhase::GpibUniversal(cmd) => RequestOp::GpibUniversalCmd { cmd },
        IoPhase::GpibAddressed => RequestOp::GpibAddressedCmd {
            data: match &plan.gpib {
                Some(GpibCycle::Addressed(frame)) => frame.clone(),
                // Unreachable by construction: `io_phases` only emits
                // `GpibAddressed` for `GpibCycle::Addressed`.
                _ => Vec::new(),
            },
        },
        IoPhase::GpibSerialPollEnable => RequestOp::GpibUniversalCmd {
            cmd: crate::interfaces::gpib::IBSPE,
        },
        // C reads the response byte through asynOctet, not asynGpib
        // (asynRecord.c:1729-1731) — one byte, into SPR.
        IoPhase::GpibSerialPollRead => RequestOp::OctetRead { buf_size: 1 },
        IoPhase::GpibSerialPollDisable => RequestOp::GpibUniversalCmd {
            cmd: crate::interfaces::gpib::IBSPD,
        },
    }
}

/// The `asynUser` one phase submits with. C gives every transfer the record's
/// TMOT (`pasynUser->timeout = pasynRec->tmot`, asynRecord.c:818) — including
/// the GPIB commands, which run under the same queued user — and only the flush
/// carries no transfer timeout.
fn io_phase_user(plan: &IoPlan, phase: IoPhase) -> AsynUser {
    match phase {
        IoPhase::Flush => flush_user(plan),
        _ => io_user(plan),
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
            // C: reportError(status, "<Iface> write error, %s",
            // pasynUser->errorMessage) — per interface and direction
            // (asynRecord.c:1378 Int32, :1414 UInt32, :1450 Float64). The
            // generic "write: {e}" lost which interface failed and prefixed the
            // Rust status debug onto the driver text.
            out.report_error(format!(
                "{} write error, {}",
                plan.iface.c_errs_name(),
                e.message()
            ));
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
        // The `%s` tail is C's `pasynUser->errorMessage` — the driver's own
        // text, which `AsynError::message()` reads through the PartialWrite
        // carrier. (`Display` would prefix it with the Rust status debug, which
        // C's ERRS never carries.) A short write that reported success has no
        // driver message at all; C leaves the stale `errorMessage` there, so
        // state what actually happened instead of pretending to a driver text.
        let detail = match &err {
            Some(e) => e.message(),
            None => format!("wrote {} of {} chars", nawt, plan.octet_out_len),
        };
        out.report_error(format!("Write error, nout={nawt}, {detail}"));
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

            // C `nbytesTransfered` — the count every read-side diagnostic and
            // field assignment below is built from. Taken before `data` is
            // moved into the IFMT-selected input field.
            let nread = data.len();

            out.eomr = Some(eom as i32);
            // NORD is the raw transfer count (C `nord = nbytesTransfered`,
            // asynRecord.c:1627) — independent of the overflow
            // NUL-truncation below and of UTF-8-lossy expansion.
            out.nord = Some(nread as i32);
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
            // every read mode. The escape is `epicsStrSnPrintEscaped` into the
            // 40-byte TINP field (`:1629`), so a long or escape-heavy read
            // reaches TINP cut at 39 characters.
            out.tinp = Some(crate::escape::escaped_from_raw(&data, TINP_SIZE));
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
            // C's two read-side diagnostics, in C's own order: the status
            // failure first (asynRecord.c:1593-1599), the overflow check after
            // (:1602-1621). Both go through `reportError`, which overwrites
            // ERRS, so on a read that both failed *and* overflowed the overflow
            // text is what the operator sees. The severities compose the other
            // way round — `recGblSetSevr` keeps the strictly higher — so the
            // MAJOR from the failure survives the MINOR from the overflow.
            if let Some(e) = &err {
                // C: reportError(status, "%s  nread %d %s", <status word>,
                // nbytesTransfered, pasynUser->errorMessage) — two spaces after
                // the status word, verbatim from the C format string. (The
                // earlier `"Error %s"` at :1583 is overwritten by this one and
                // never reaches the operator.)
                out.report_error(format!(
                    "{}  nread {} {}",
                    c_read_status_word(e),
                    nread,
                    e.message()
                ));
                // C performOctetIO read error -> recGblSetSevr(READ_ALARM,
                // MAJOR) (asynRecord.c:1599).
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
            if overflow {
                // C: reportError(status, "Overflow nread %d %s",
                // nbytesTransfered, pasynUser->errorMessage) in all three
                // overflow branches (asynRecord.c:1602-1621), alongside the
                // MINOR alarm. The `%s` tail is the driver's message; an
                // overflow that did not *fail* has none, and C then splices
                // whatever is left in its long-lived `pasynUser` — the empty
                // string for a record that has not yet seen an error. Take this
                // cycle's error text when there is one and the empty string
                // otherwise: the port does not carry a stale diagnostic across
                // cycles.
                let tail = err.as_ref().map(|e| e.message()).unwrap_or_default();
                out.report_error(format!("Overflow nread {nread} {tail}"));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Minor);
            }
        }
        // The three scalar reads share one rule for a driver that answers `Ok`
        // without a typed value. C cannot express that state — `read(&value)`
        // fills the out-parameter whenever it returns `asynSuccess` — so there
        // is no C diagnostic to port: the readback field keeps its previous
        // value and ERRS stays untouched. (An invented "returned no value" text
        // used to reach the operator from the Int32 and Float64 arms only, while
        // UInt32Digital was already silent.)
        InterfaceType::Int32 => match res {
            Ok(result) => {
                if let Some(v) = result.int_val {
                    out.i32inp = Some(v);
                }
            }
            // C performInt32IO read error -> recGblSetSevr(READ_ALARM, MAJOR)
            // (asynRecord.c:1393).
            Err(e) => {
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(format!(
                    "{} read error, {}",
                    plan.iface.c_errs_name(),
                    e.message()
                ));
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
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(format!(
                    "{} read error, {}",
                    plan.iface.c_errs_name(),
                    e.message()
                ));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
        InterfaceType::Float64 => match res {
            Ok(result) => {
                if let Some(v) = result.float_val {
                    out.f64inp = Some(v);
                }
            }
            // C performFloat64IO read error -> recGblSetSevr(READ_ALARM, MAJOR)
            // (asynRecord.c:1465).
            Err(e) => {
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(format!(
                    "{} read error, {}",
                    plan.iface.c_errs_name(),
                    e.message()
                ));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
    }
}

/// Whether the `performIO` cycle continues after a phase.
///
/// A phase that *ran* and failed reports and the cycle carries on, exactly as C
/// `performIO` does (a failed write is still followed by the read). A phase that
/// never ran — its queued request was removed by the queue-wait deadline — ends
/// the cycle: in C that request never entered `performIO` at all, and what runs
/// instead is `queueTimeoutCallbackProcess`.
#[must_use]
enum PhaseFlow {
    Continue,
    Aborted,
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
) -> PhaseFlow {
    // The queue-wait deadline, ahead of every per-phase recorder — including the
    // flush's "discard the status" one, which would otherwise swallow it. This is
    // not an I/O failure to report as one: the request never reached the driver,
    // so there is no transfer to publish (no NAWT, no NORD, no EOMR) and no
    // driver diagnostic to splice. C reports it from a different callback
    // entirely (`queueTimeoutCallbackProcess`), which is what this is.
    if let Err(e) = &res {
        if e.is_queue_timeout() {
            out.report_queue_timeout();
            return PhaseFlow::Aborted;
        }
        // The queue gate's refusal, ahead of the per-phase recorders for the same
        // reason: C's `queueRequest` returned `asynDisabled` / `asynDisconnected`
        // in `process()` (asynRecord.c:342-355) and `performIO` was never entered,
        // so no phase of this plan ran and none of them may report. C's own
        // `performIO` is one queued request covering every phase, so a refusal
        // aborts the whole plan here, not just this phase.
        if e.is_queue_refused() {
            out.report_queue_refused();
            return PhaseFlow::Aborted;
        }
    }
    match phase {
        IoPhase::Flush => {
            // C `performOctetIO` calls the flush for its side effect only and
            // discards the status — the call is a bare statement, not assigned
            // to `status` (asynRecord.c:1521), unlike every other transfer in
            // the routine. So a flush failure never reaches `reportError` and
            // never reaches ERRS.
            //
            // That is not an oversight to "improve on": the flush is
            // best-effort housekeeping before the write, and reporting it gave
            // the port a diagnostic C does not have — one that a *successful*
            // write/read afterwards would not clear, so a whole good
            // transaction could carry a stale "flush: ..." string. Dropping the
            // result here is what keeps ERRS meaning "this transfer failed".
            let _ = res;
        }
        IoPhase::Write => record_write_result(plan, out, res),
        IoPhase::Read => record_read_result(plan, out, res),
        IoPhase::GpibUniversal(_) => {
            // C gpibUniversalCmd (asynRecord.c:1672-1677).
            if let Err(e) = res {
                out.report_error(format!("GPIB Universal command {}", e.message()));
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibAddressed => {
            // C gpibAddressedCmd (asynRecord.c:1750-1756).
            if let Err(e) = res {
                out.report_error(format!(
                    "Error in GPIB Addressed Command write, {}",
                    e.message()
                ));
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollEnable => {
            // C Serial Poll step 1 (asynRecord.c:1721-1726).
            if let Err(e) = res {
                out.report_error(format!("Error in GPIB Serial Poll write, {}", e.message()));
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollRead => {
            // C Serial Poll step 2 (asynRecord.c:1728-1735): the response byte
            // lands in SPR, and *either* a failing status *or* a transfer of
            // other than one byte is the error — the record keeps the last SPR
            // in that case, because C only ever wrote through the pointer if the
            // driver moved a byte.
            let (data, err) = match res {
                Ok(result) => (result.data.unwrap_or_default(), None),
                Err(e) => (
                    e.partial_read().map(|p| p.data.clone()).unwrap_or_default(),
                    Some(e),
                ),
            };
            if let Some(byte) = data.first() {
                out.spr = Some(i32::from(*byte));
            }
            if err.is_some() || data.len() != 1 {
                // C's `%s` tail is `pasynUser->errorMessage`; a short read that
                // reported success leaves it as `resetError` left it — empty.
                let detail = err.map(|e| e.message()).unwrap_or_default();
                out.report_error(format!("Error in GPIB Serial Poll read, {detail}"));
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollDisable => {
            // C Serial Poll step 3 (asynRecord.c:1737-1744).
            if let Err(e) = res {
                out.report_error(format!(
                    "Error in GPIB Serial Poll disable write, {}",
                    e.message()
                ));
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
    }
    PhaseFlow::Continue
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
        out.report_canceled();
        return out;
    }

    for phase in io_phases(&plan) {
        let res = handle
            .submit_cancellable(
                io_phase_op(&plan, phase),
                io_phase_user(&plan, phase),
                cancel.clone(),
            )
            .await;
        if cancel.is_cancelled() {
            out.report_canceled();
            return out;
        }
        if let PhaseFlow::Aborted = record_phase_result(&plan, &mut out, phase, res) {
            return out;
        }
    }

    out
}

/// C `reportError(pasynRec, status, "I/O request canceled")` for a dequeued
/// `AQR` request (asynRecord.c:398).
const CANCELED_MSG: &str = "I/O request canceled";

/// C `QUEUE_TIMEOUT` (asynRecord.c:71): how long a record request may wait in
/// the port queue before it is removed and reported as never having run.
///
/// The record passes it to **every** `queueRequest` it makes — the process I/O
/// (:343), the special option/EOS callback (:572), and the getOption/getEos
/// requests `connectDevice` queues (:1281,:1297) — and it is the only caller in
/// asyn that asks for one at all (device support passes 0.0, devAsynInt32.c:838).
/// So it belongs to the record, not to the port: it rides on
/// [`AsynUser::queue_timeout`] of the users the record builds below.
const QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// C `queueTimeoutCallbackProcess` (asynRecord.c:919-926) — the process request
/// waited out `QUEUE_TIMEOUT` and was removed from the queue.
const PROCESS_QUEUE_TIMEOUT_MSG: &str = "process queueRequest timeout";

/// C `process()` (asynRecord.c:355) — the queue gate refused the process request,
/// so `performIO` never ran.
const PROCESS_QUEUE_REFUSED_MSG: &str = "queueRequest failed";

/// C `queueTimeoutCallbackSpecial` (asynRecord.c:929-938) — the special
/// (option / EOS / connect readback) request waited out `QUEUE_TIMEOUT`.
/// Unlike the process one, C raises **no** record severity here: it reports,
/// returns the record to `stateIdle`, and frees the request.
const SPECIAL_QUEUE_TIMEOUT_MSG: &str = "special queueRequest timeout";

/// The fields asynRecord.dbd marks `special(SPC_MOD)` — the complete set of puts
/// that reach C `special()` (dbAccess only calls `special` for an SPC_MOD field).
///
/// This port's database calls [`Record::special`] after every accepted put, so
/// the C set has to be stated: it is the gate in [`AsynRecord::special`], and the
/// dispatch arms below it are the same 43 names. A field added to one must be
/// added to the other — `spc_mod_fields_match_the_dbd` pins the list to the dbd.
const SPC_MOD_FIELDS: &[&str] = &[
    "PORT", "ADDR", "PCNCT", "DRVINFO", "REASON", "IFACE", "OEOS", "IEOS", "UI32MASK", "BAUD",
    "LBAUD", "PRTY", "DBIT", "SBIT", "MCTL", "FCTL", "IXON", "IXOFF", "IXANY", "HOSTINFO", "DRTO",
    "TMSK", "TB0", "TB1", "TB2", "TB3", "TB4", "TB5", "TIOM", "TIB0", "TIB1", "TIB2", "TINM",
    "TINB0", "TINB1", "TINB2", "TINB3", "TSIZ", "TFIL", "AUCT", "CNCT", "ENBL", "AQR",
];

/// The fields C `getOptions` re-reads from the driver and POST_IF_NEWs
/// (asynRecord.c:1834-1938: `REMEMBER_STATE` on each, then `POST_IF_NEW` after
/// the `getOption` calls). Same set as [`AsynRecord::read_options_from_driver`]
/// writes — the two must stay in step, or an option would be refreshed without
/// its monitor firing.
const OPTION_READBACK_FIELDS: &[&str] = &[
    "BAUD", "LBAUD", "PRTY", "DBIT", "SBIT", "MCTL", "FCTL", "IXON", "IXOFF", "IXANY", "HOSTINFO",
    "DRTO",
];

/// The driver option keys C `getOptions` reads (asynRecord.c:1864-1937), in C's
/// order. The record reads them in one pass so a queue-wait timeout — which in C
/// means the single request carrying *all* of them never ran — can abort the
/// whole readback rather than half of it.
const OPTION_READBACK_KEYS: &[&str] = &[
    "baud",
    "parity",
    "bits",
    "stop",
    "crtscts",
    "clocal",
    "ixon",
    "ixoff",
    "ixany",
    "hostinfo",
    "disconnectOnReadTimeout",
];

/// The fields C `monitorStatus` POST_IF_NEWs (asynRecord.c:1102-1141) — the
/// readbacks it re-imports from the trace manager and the port, plus the two it
/// posts because `special()` changed them behind the operator's back (REASON and
/// the DRVINFO its put blanks, :488-489).
///
/// TFIL is in the list: C posts it from the same function, substituting
/// "Unknown" when another thread re-pointed the trace file (:1119-1124).
const MONITOR_STATUS_FIELDS: &[&str] = &[
    "TMSK", "TB0", "TB1", "TB2", "TB3", "TB4", "TB5", "TIOM", "TIB0", "TIB1", "TIB2", "TINM",
    "TINB0", "TINB1", "TINB2", "TINB3", "TSIZ", "TFIL", "AUCT", "CNCT", "PCNCT", "REASON",
    "DRVINFO", "ENBL", "OCTETIV", "OPTIONIV", "GPIBIV", "I32IV", "UI32IV", "F64IV",
];

/// The driver option key C's HOSTINFO field writes (asynRecord.c:1802-1806) —
/// the one option put C queues on a port that is not connected.
const HOSTINFO_OPTION_KEY: &str = "hostinfo";

/// Which queue class an option request belongs to — C's choice between
/// `queueRequest(..., asynQueuePriorityLow, ...)` and
/// `queueRequest(..., asynQueuePriorityConnect, ...)` with
/// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` on the user.
///
/// C makes that choice per *field* (asynRecord.c:565-569), so it is made here
/// per option *key* and nowhere else: [`Self::for_key`] is the single owner of
/// "may this option request run on a disconnected port".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionQueue {
    /// Low priority — refused while the port is disconnected.
    Normal,
    /// Connect priority carrying `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`.
    EvenIfNotConnected,
}

impl OptionQueue {
    fn for_key(key: &str) -> Self {
        if key == HOSTINFO_OPTION_KEY {
            Self::EvenIfNotConnected
        } else {
            Self::Normal
        }
    }
}

/// The fields C `getEos` re-reads and posts (asynRecord.c:2016-2024) — both
/// EOS strings, whichever one was written.
const EOS_READBACK_FIELDS: &[&str] = &["IEOS", "OEOS"];

/// Did an `asynCallbackSpecial` body run? See [`AsynRecord::special_callback`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpecialRan {
    /// The queued request ran — C's switch falls out to `monitorStatus`
    /// (asynRecord.c:897), including from an arm's error `break` and from
    /// `setOption`'s "No asynOption interface" return.
    Yes,
    /// The request never left the queue, so `asynCallbackSpecial` never ran at
    /// all: C dispatches `queueTimeoutCallbackSpecial` instead (:929-938), which
    /// reports the timeout and frees the user without touching `monitorStatus`.
    No,
}

/// C `monitorStatus`'s trace half (asynRecord.c:1066-1124), sampled once: the
/// three masks, the I/O truncate size, and C's "another thread re-pointed the
/// trace file" verdict.
///
/// The single owner of that sample. Both refresh paths — the in-record
/// [`AsynRecord::read_trace_state`] and the out-of-band `exceptCallback` post —
/// take it from here, so neither can refresh a subset of what C refreshes.
#[derive(Clone, Copy)]
struct TraceReadback {
    trace_mask: u32,
    io_mask: u32,
    info_mask: u32,
    truncate_size: i32,
    /// C: `traceFd != old.traceFd` (asynRecord.c:1119) — the record did not
    /// install this sink itself, so it cannot know the file's name.
    file_changed: bool,
}

/// Sample the trace manager and settle the TFIL verdict against the record's
/// remembered sink identity — C's `old.traceFd` (asynRecord.c:1101,1119-1120).
///
/// The remembered id is updated here (C updates it in the same `if` body) and
/// by [`AsynRecord::apply_trace_file`], which stores the id of the sink it is
/// about to install *before* installing it — C does exactly that, and for the
/// same reason: `setTraceFile` fires an exception whose callback re-enters this
/// sample, and the record's own write must not be reported as foreign
/// (asynRecord.c:470-475). A first sample (no id remembered yet) seeds the cache
/// and reports no change: C's zero-initialised `old.traceFd` matches the default
/// errlog sink's `FILE *` of 0, so its first `monitorStatus` is silent too.
fn sample_trace_readback(
    trace: &TraceManager,
    port: &str,
    addr: Option<i32>,
    old_file_id: &Mutex<Option<usize>>,
) -> TraceReadback {
    let snap = trace.snapshot(port, addr);
    let file_changed = match old_file_id.lock() {
        Ok(mut cached) => cached
            .replace(snap.file_id)
            .is_some_and(|old| old != snap.file_id),
        Err(_) => false,
    };
    TraceReadback {
        trace_mask: snap.trace_mask.bits(),
        io_mask: snap.io_mask.bits(),
        info_mask: snap.info_mask.bits(),
        truncate_size: snap.io_truncate_size as i32,
        file_changed,
    }
}

/// The TFIL text C writes when the trace file changed under the record
/// (asynRecord.c:1122).
const TFIL_UNKNOWN: &str = "Unknown";

/// Build the asyn trace readback fields from the three trace masks — the
/// single source of the mask→field mapping that C `monitorStatus`
/// recomputes and posts (asynRecord.c:1066-1117). The bit assignments mirror
/// [`AsynRecord::update_trace_bits_from_mask`] /
/// [`AsynRecord::update_io_bits_from_mask`] /
/// [`AsynRecord::update_info_bits_from_mask`] and reference the same
/// `Trace*Mask` constants, so the out-of-band trace post and the
/// `process()`-path re-import (`read_trace_state`) cannot diverge. Field DBF
/// types match `get_field`: the mask fields are `Long`, the bit fields are
/// `DBF_MENU` (`menu(asynTRACE)`, asynRecord.dbd:481-577) and so are posted as
/// `Enum` — a post must carry the field's native type or the monitor delivers a
/// value the client cannot decode against the field's declared type.
fn trace_readback_fields(rb: &TraceReadback) -> Vec<(String, EpicsValue)> {
    let TraceReadback {
        trace_mask,
        io_mask,
        info_mask,
        truncate_size,
        file_changed,
    } = *rb;
    let bit = |mask: u32, flag: u32| EpicsValue::Enum(u16::from(mask & flag != 0));
    let mut fields = vec![
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
        // C: `tsiz = (int)getTraceIOTruncateSize(pasynUser)` then POST_IF_NEW
        // (asynRecord.c:1100,:1118).
        ("TSIZ".to_string(), EpicsValue::Long(truncate_size)),
    ];
    if file_changed {
        // C: the trace file is no longer the one this record installed, so its
        // name is unknowable (asynRecord.c:1119-1124).
        fields.push(("TFIL".to_string(), EpicsValue::String(TFIL_UNKNOWN.into())));
    }
    fields
}

/// The port-state half of C `monitorStatus`: AUCT / CNCT / ENBL re-read from
/// `pasynManager->isAutoConnect` / `isConnected` / `isEnabled`
/// (asynRecord.c:1085-1099) and POST_IF_NEWed (:1125-1133). A failing query
/// gives 0, exactly as C's `else` branches do.
///
/// Sibling of [`trace_readback_fields`]: together they are everything
/// `exceptCallback` refreshes, and the same list the `process()`-path
/// [`AsynRecord::monitor_status`] writes. Field DBF types match `get_field`
/// (all three are `DBF_MENU`: asynRecord.dbd:599, :606, :613).
fn connect_readback_fields(
    auto_connect: bool,
    connected: bool,
    enabled: bool,
) -> Vec<(String, EpicsValue)> {
    vec![
        (
            "AUCT".to_string(),
            EpicsValue::Enum(u16::from(auto_connect)),
        ),
        ("CNCT".to_string(), EpicsValue::Enum(u16::from(connected))),
        ("ENBL".to_string(), EpicsValue::Enum(u16::from(enabled))),
    ]
}

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
            Err(_) => {
                // C reports the path alone — `fopen` leaves no message to splice
                // (asynRecord.c:465-466).
                self.report_error(format!("Error opening trace file: {tfil}"));
                return;
            }
        };
        // C stores the new `FILE *` in `old.traceFd` BEFORE calling setTraceFile,
        // because setTraceFile fires an exception whose callback runs
        // monitorStatus — which would otherwise see the record's own write as a
        // foreign change and overwrite TFIL with "Unknown" (asynRecord.c:470-475).
        if let Ok(mut cached) = self.old_trace_file_id.lock() {
            *cached = Some(file.id());
        }
        match self.trace_addr_target() {
            Some(addr) => entry.trace.set_device_trace_file(&self.port, addr, file),
            None => entry.trace.set_trace_file(Some(&self.port), file),
        }
    }

    /// Read current trace state from TraceManager into record fields.
    ///
    /// C `monitorStatus` (asynRecord.c:1066-1124) refreshes the trace mask, the
    /// trace I/O mask, the trace info mask (`TINM`/`TINB0..3`), the I/O truncate
    /// size (`TSIZ`, :1100), and `TFIL` — which becomes "Unknown" whenever the
    /// port's trace sink is no longer the one this record installed (:1119-1124).
    /// The sample and the TFIL verdict come from [`sample_trace_readback`], the
    /// same owner the out-of-band `exceptCallback` refresh uses.
    fn read_trace_state(&mut self) {
        let Some(entry) = self.port_entry.clone() else {
            return;
        };
        let addr = self.trace_addr_target();
        let cache = Arc::clone(&self.old_trace_file_id);
        let rb = sample_trace_readback(&entry.trace, &self.port, addr, &cache);

        self.tmsk = rb.trace_mask as i32;
        self.update_trace_bits_from_mask();

        self.tiom = rb.io_mask as i32;
        self.update_io_bits_from_mask();

        self.tinm = rb.info_mask as i32;
        self.update_info_bits_from_mask();

        self.tsiz = rb.truncate_size;

        if rb.file_changed {
            self.tfil = TFIL_UNKNOWN.to_string();
        }
    }

    /// Subscribe to the port's exceptions — C `exceptCallback`
    /// (asynRecord.c:903-917).
    ///
    /// C registers it with `exceptionCallbackAdd` in `connectDevice`
    /// (asynRecord.c:1269) and takes *every* `asynException`: the callback body
    /// is an unconditional `monitorStatus(pasynRec)` under `dbScanLock`, with
    /// the comment "There has been a change in connect or enable status". So a
    /// port going down, an `asynSetAutoConnect`, an `asynEnable` — as much as an
    /// `asynSetTraceMask` — re-imports the readback fields and posts the changed
    /// ones immediately, out of band; none of them waits for the next
    /// `process()`. The port subscribed only to the three Trace* exceptions and
    /// refreshed only the trace masks, so CNCT/AUCT/ENBL sat stale until the
    /// record happened to process again (and for a passive record with no scan,
    /// indefinitely — a dead link still read "Connected").
    ///
    /// The Rust analogue posts through the merged PACT seam: when the record
    /// carries a database handle (`async_ctx`) and a runtime is available, the
    /// callback recomputes the readback fields — trace masks from the trace
    /// manager, AUCT/CNCT/ENBL from the port — and `post_fields`-es them now
    /// (the C `db_post_events` under `dbScanLock`). Like C `POST_IF_NEW`
    /// (asynRecord.c:210-214) it keeps a last-posted cache and posts only the
    /// fields whose value changed, so an unrelated exception does not re-post
    /// unchanged readback fields. With no handle / runtime — e.g. a record
    /// connected outside a database — it falls back to raising a dirty flag that
    /// `process()` drains through the single [`Self::monitor_status`] owner.
    ///
    /// The port-state queries are async, deliberately. The exception is
    /// announced from *inside* the driver, i.e. on the port actor's own thread
    /// (`PortDriverBase::set_connected` → `announce_exception`), so an
    /// `is_connected_blocking()` here would be the actor waiting on itself.
    /// The refresh is spawned onto the runtime, where the queries queue behind
    /// the op that raised the exception and are served when the actor loops.
    fn register_exception_callback(&mut self) {
        self.clear_exception_callback();
        let Some(ref entry) = self.port_entry else {
            return;
        };
        let Some(mgr) = entry.trace.exception_manager() else {
            return;
        };
        let port = self.port.clone();
        let trace = entry.trace.clone();
        let handle = entry.handle.clone();
        let dirty = Arc::clone(&self.status_dirty);
        // The immediate out-of-band post needs both a database handle (to post
        // through) and a runtime handle (the exception fires from the thread
        // that raised it — iocsh, or the port actor itself — which is not a
        // tokio worker, so `tokio::spawn` would panic; an explicit `Handle`
        // submits to the runtime from any thread). Capture them once here,
        // where registration runs in the database's async init context.
        let immediate = match (
            self.async_ctx.clone(),
            tokio::runtime::Handle::try_current().ok(),
        ) {
            (Some((name, db)), Some(rt)) => Some((name, db, rt)),
            _ => None,
        };
        // C `monitorStatus` posts a readback field only when it differs from the
        // per-record remembered value (`POST_IF_NEW`, asynRecord.c:210-214,
        // :1102-1133). The base `post_fields` path posts unconditionally, so the
        // out-of-band re-post keeps its own last-posted cache — the asynRecord
        // `old` analogue — to avoid re-posting unchanged fields on every
        // exception. Seed it with the record's current values: `connect_device`
        // registers this callback only after its own `monitor_status`, so those
        // are the values C's `old` would hold at the same point.
        let old_file_id = Arc::clone(&self.old_trace_file_id);
        // The rung of C's findTracePvt chain this record's trace reads and writes
        // both address (device when the port is multi-device, else the port).
        let trace_addr = self.trace_addr_target();
        let last_posted: Arc<Mutex<HashMap<String, EpicsValue>>> = Arc::new(Mutex::new(
            trace_readback_fields(&sample_trace_readback(
                &trace,
                &port,
                trace_addr,
                &old_file_id,
            ))
            .into_iter()
            .chain(connect_readback_fields(
                self.auct != 0,
                self.cnct != 0,
                self.enbl != 0,
            ))
            .collect(),
        ));
        let id = mgr.add_callback(move |ev| {
            // C `exceptCallback` filters on nothing: its body is an
            // unconditional `monitorStatus` (asynRecord.c:913), so every
            // exception on the record's port refreshes every readback field.
            // The subscription is per-port in C (the callback hangs off the
            // record's `pasynUser`), which is what this name check reproduces.
            if ev.port_name != port {
                return;
            }
            let Some((name, db, rt)) = immediate.clone() else {
                dirty.store(true, Ordering::Release);
                return;
            };
            let (port, trace, handle, last_posted, old_file_id) = (
                port.clone(),
                trace.clone(),
                handle.clone(),
                Arc::clone(&last_posted),
                Arc::clone(&old_file_id),
            );
            rt.spawn(async move {
                // The C `monitorStatus` body: re-import the trace masks from the
                // trace manager and re-read the port's auto-connect / connect /
                // enable state (asynRecord.c:1066-1099), then POST_IF_NEW the
                // fields that changed (:1102-1133). The port queries are async
                // because this refresh can be driven from the actor thread — see
                // the doc comment.
                let auto = handle.is_auto_connect().await.unwrap_or(false);
                let connected = handle.is_connected().await.unwrap_or(false);
                let enabled = handle.is_enabled().await.unwrap_or(false);
                let fields = trace_readback_fields(&sample_trace_readback(
                    &trace,
                    &port,
                    trace_addr,
                    &old_file_id,
                ))
                .into_iter()
                .chain(connect_readback_fields(auto, connected, enabled));
                let changed: Vec<(String, EpicsValue)> = {
                    let mut cache = last_posted.lock().unwrap();
                    let mut changed = Vec::new();
                    for (field, value) in fields {
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
                let _ = db.post_fields(&name, changed);
            });
        });
        self.except_cb = Some((mgr, id));
    }

    /// Remove the trace exception subscription (C `exceptionCallbackRemove`,
    /// asynRecord.c:523,1154,1313). Idempotent.
    fn clear_exception_callback(&mut self) {
        if let Some((mgr, id)) = self.except_cb.take() {
            mgr.remove_callback(id);
        }
    }

    /// Read serial/IP options from the driver into record fields — C
    /// `getOptions` (asynRecord.c:1834-1938). It is the *only* writer of the
    /// [`OPTION_READBACK_FIELDS`] from the driver side, and it runs on every
    /// option put (see [`Self::write_option`]) as well as on connect, so those
    /// fields always show the driver's actual value rather than the requested
    /// one.
    fn read_options_from_driver(&mut self, handle: &PortHandle, queue: OptionQueue) {
        // C returns immediately when the port carries no asynOption interface
        // (asynRecord.c:1843-1844) — the record keeps the values it had.
        if !handle.has_interface(crate::interfaces::InterfaceType::Option) {
            return;
        }
        // Every `getOption` C's `getOptions` runs happens *inside* one queued
        // request (`callbackGetOption`), so they stand or fall together: if that
        // request never ran, no option was read. That also fixes their queue
        // class: the readback inherits the gate of the request that carries it,
        // which is why `queue` is the caller's and not this function's to pick.
        let Some(opts) = self.get_options(handle, OPTION_READBACK_KEYS, queue) else {
            return;
        };
        // C hands `getOption` a buffer the driver clears at entry
        // (`val[0] = '\0'`, drvAsynSerialPort.c:142, drvAsynIPPort.c:894) and
        // then ignores the returned status (asynRecord.c:1863-1928). So a key the
        // port does not implement is an *empty readback*, not a skipped field: an
        // IP port answers "" for `parity`, and PRTY lands on its Unknown choice
        // instead of keeping a stale value from the serial port the record used
        // to point at. One rule for every key.
        let opt = |key: &str| opts.get(key).map(String::as_str).unwrap_or("");

        // Baud rate. C derives both fields from one text:
        // `sscanf(optbuff, "%d", &pasynRec->lbaud)` and the `baud_choices` walk
        // (asynRecord.c:1866-1871). BAUD is matched against the choice *text*, so
        // a driver reporting a rate no menu choice carries leaves BAUD at
        // "Unknown" while LBAUD still shows the number.
        //
        // LBAUD is the one field C does *not* pre-zero: `sscanf` writes nothing
        // when the text carries no number, so LBAUD keeps its previous value —
        // where the port's `parse().unwrap_or(0)` reported the line as 0 baud.
        // The parse is C's `%d`, a prefix parse (`option_parse::sscanf_int`).
        let baud_text = opt("baud");
        if let Some(rate) = crate::drivers::option_parse::sscanf_int(baud_text) {
            self.lbaud = rate;
        }
        self.baud = baud_choice_index(baud_text);
        // Parity
        self.prty = match opt("parity") {
            "none" => 1,
            "even" => 2,
            "odd" => 3,
            _ => 0, // unknown
        };
        // Data bits. The serial driver's get_option exposes data bits
        // under the key `"bits"` (C asynRecord.c:1884 likewise reads
        // "bits"); `"csize"` is consumed by no driver, so the readback
        // must use "bits" to match the DBIT write path (write_option
        // "bits" below).
        self.dbit = match opt("bits") {
            "5" => 1,
            "6" => 2,
            "7" => 3,
            "8" => 4,
            _ => 0,
        };
        // Stop bits
        self.sbit = match opt("stop") {
            "1" => 1,
            "2" => 2,
            _ => 0,
        };
        // Flow control
        self.fctl = match opt("crtscts") {
            "Y" | "Yes" => 2,         // Hardware
            "N" | "No" | "none" => 1, // None
            _ => 0,
        };
        // Modem control
        self.mctl = match opt("clocal") {
            "Y" | "Yes" => 1, // CLOCAL
            "N" | "No" => 2,  // YES (hardware modem control)
            _ => 0,
        };
        // XON/XOFF
        self.ixon = match opt("ixon") {
            "Y" | "Yes" => 2,
            "N" | "No" => 1,
            _ => 0,
        };
        self.ixoff = match opt("ixoff") {
            "Y" | "Yes" => 2,
            "N" | "No" => 1,
            _ => 0,
        };
        self.ixany = match opt("ixany") {
            "Y" | "Yes" => 2,
            "N" | "No" => 1,
            _ => 0,
        };
        // IP options. C `strncpy(pasynRec->hostinfo, hostbuff, …)`
        // (asynRecord.c:1928) copies the buffer whatever the get returned, so a
        // port carrying no `hostinfo` key clears the field.
        self.hostinfo = opt("hostinfo").to_string();
        self.drto = match opt("disconnectOnReadTimeout") {
            "Y" | "Yes" => 2,
            "N" | "No" => 1,
            _ => 0,
        };
    }

    /// Read the driver's options under the record's queued `AsynUser`.
    ///
    /// A key the driver does not know leaves its field alone — C's `getOption`
    /// failure branch reports nothing and the record keeps what it had. A
    /// **queue-wait timeout** is different in kind: the request never reached the
    /// driver, so nothing was read and the whole readback is off. C reports that
    /// through `queueTimeoutCallbackSpecial` (asynRecord.c:929-938) and the
    /// `getOptions` inside that request simply never happen — `None` here.
    fn get_options(
        &mut self,
        handle: &PortHandle,
        keys: &[&str],
        queue: OptionQueue,
    ) -> Option<HashMap<String, String>> {
        let mut opts = HashMap::new();
        for key in keys {
            match handle.get_option_blocking(self.option_user_for(queue), key) {
                Ok(val) => {
                    opts.insert((*key).to_string(), val);
                }
                Err(e) if e.is_queue_timeout() => {
                    self.report_special_queue_timeout();
                    return None;
                }
                Err(_) => {}
            }
        }
        Some(opts)
    }

    /// Read both EOS strings back from the driver into IEOS/OEOS — C `getEos`
    /// (asynRecord.c:1985-2026). The single driver-side writer of
    /// [`EOS_READBACK_FIELDS`], run after every EOS put (see the `IEOS`/`OEOS`
    /// arms of `special`).
    ///
    /// C seeds both escaped buffers to `""` and overwrites a buffer only when
    /// the corresponding `getInputEos`/`getOutputEos` succeeds *and* returns a
    /// non-empty EOS, so a port with no asynOctet interface, a failing get, or
    /// a driver holding no EOS all land as an empty field. The escaping is
    /// `epicsStrSnPrintEscaped` into an [`EOS_SIZE`]-byte buffer (`:1990-1991`,
    /// `:2005`, `:2012`) — the same transform TINP uses, under a *tighter*
    /// bound, and the exact inverse of the `translate_escape` the put path
    /// applies.
    fn read_eos_from_driver(&mut self, handle: &PortHandle) {
        let mut ieos = String::new();
        let mut oeos = String::new();
        if self.octetiv != 0 {
            // As in `get_options`: a queue-wait timeout means the request never
            // ran, so neither EOS was read and the fields must not be rewritten
            // from a readback that did not happen.
            let read = |res: AsynResult<Vec<u8>>| -> Result<String, bool> {
                match res {
                    Ok(bytes) if !bytes.is_empty() => {
                        Ok(crate::escape::escaped_from_raw(&bytes, EOS_SIZE))
                    }
                    Ok(_) => Ok(String::new()),
                    Err(e) if e.is_queue_timeout() => Err(true),
                    Err(_) => Ok(String::new()),
                }
            };
            match read(handle.get_input_eos_blocking(self.option_user())) {
                Ok(v) => ieos = v,
                Err(_) => {
                    self.report_special_queue_timeout();
                    return;
                }
            }
            match read(handle.get_output_eos_blocking(self.option_user())) {
                Ok(v) => oeos = v,
                Err(_) => {
                    self.report_special_queue_timeout();
                    return;
                }
            }
        }
        self.ieos = ieos;
        self.oeos = oeos;
    }

    /// Snapshot the named fields' current values — C `REMEMBER_STATE`
    /// (asynRecord.c:1850-1862, :1999-2000): the "before" half of a
    /// driver-readback POST_IF_NEW. Feed the result to [`Self::post_if_new`]
    /// after the readback has written the fields.
    fn field_snapshot(&self, names: &[&str]) -> Vec<(String, EpicsValue)> {
        names
            .iter()
            .filter_map(|name| self.get_field(name).map(|v| ((*name).to_string(), v)))
            .collect()
    }

    /// Post the fields whose value changed across a driver readback — C
    /// `POST_IF_NEW` (asynRecord.c:210-214) as `getOptions` / `getEos` use it.
    ///
    /// Posting is out of band (the record lock is held by the `special` caller,
    /// and `post_fields` is async), matching the C `db_post_events` those two
    /// functions issue from inside the callback rather than from `process`.
    /// Without a database handle / runtime — a record driven outside a database,
    /// as in the unit tests — the field values are still refreshed; only the
    /// monitor post is unavailable.
    /// Run a driver readback inside C's `REMEMBER_STATE … POST_IF_NEW` bracket
    /// (asynRecord.c:210-214): snapshot the fields it may write, run it, post the
    /// ones that changed.
    ///
    /// The single owner of "a readback that moved a field fires its monitor".
    /// Every place that re-reads record fields from the port goes through it —
    /// `getOptions` (:1850-1938), `getEos` (:1999-2024), `monitorStatus`
    /// (:1042-1141) — because C posts from *inside* each of those, not from the
    /// caller. `connectDevice` refreshed all three and posted none of them
    /// (R14-47): the operator's screen kept the previous port's BAUD/IEOS/CNCT
    /// until something else happened to fire a monitor.
    fn posting<T>(&mut self, fields: &[&str], readback: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.field_snapshot(fields);
        let out = readback(self);
        self.post_if_new(&before);
        out
    }

    fn post_if_new(&self, before: &[(String, EpicsValue)]) {
        let changed: Vec<(String, EpicsValue)> = before
            .iter()
            .filter_map(|(field, old)| {
                let new = self.get_field(field)?;
                (new != *old).then(|| (field.clone(), new))
            })
            .collect();
        if changed.is_empty() {
            return;
        }
        let (Some((name, db)), Ok(rt)) = (
            self.async_ctx.clone(),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        rt.spawn(async move {
            let _ = db.post_fields(&name, changed);
        });
    }

    /// Write a serial/IP option to the driver via SetOption, then re-read every
    /// option back from the driver.
    ///
    /// C `asynCallbackSpecial` falls through `callbackSetOption` into
    /// `callbackGetOption` (asynRecord.c:845-849, the `/* no break */`), so a
    /// `setOption` is *always* followed by `getOptions` — even when the set
    /// failed. That is what makes the option fields a readback of the driver
    /// rather than a latch of the operator's request: a driver that rounds 9601
    /// to 9600, or rejects the baud outright, snaps the field to what it really
    /// holds. Skipping the re-read left the record advertising a line setting
    /// the hardware never adopted.
    ///
    /// C `setOption` reports a failing `pasynOption->setOption` as
    /// `"Error setting option, %s"` with `pasynUser->errorMessage`
    /// (asynRecord.c:1825-1828) — it never names the key, and it raises no
    /// record severity. The port invented `"set_option({key}): {e}"`, which
    /// also prefixed the Rust status debug onto the driver text.
    fn write_option(&mut self, key: &str, value: &str) {
        let (key, value) = (key.to_string(), value.to_string());
        self.special_callback(|this| this.write_option_body(&key, &value));
    }

    /// The `setOption` arm of C's `asynCallbackSpecial` (asynRecord.c:843-849).
    /// Its tail — `monitorStatus` — is [`Self::special_callback`]'s.
    fn write_option_body(&mut self, key: &str, value: &str) -> SpecialRan {
        let Some(entry) = self.port_entry.clone() else {
            // No port: `special()` never queued the callback, so nothing ran.
            return SpecialRan::No;
        };
        // C setOption (asynRecord.c:1766-1771): a port with no asynOption
        // interface takes the same refusal as a missing I/O interface. The
        // `return` is out of `setOption`, not out of `asynCallbackSpecial` — the
        // status repost still happens.
        if !self.port_has(crate::interfaces::InterfaceType::Option) {
            self.report_no_interface("asynOption");
            return SpecialRan::Yes;
        }
        // C `special()` queues every option put at `asynQueuePriorityLow` — with
        // one exception it calls out by name: HOSTINFO goes at
        // `asynQueuePriorityConnect` carrying
        // `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`, "Enable changing host:port
        // when not connected" (asynRecord.c:565-569). It is the operator's only
        // route to repoint an IP port that is aimed at a wrong or moved host,
        // and such a port is disconnected by definition — refusing the put is
        // refusing the repair.
        let queue = OptionQueue::for_key(key);
        if let Err(e) = entry
            .handle
            .set_option_blocking(self.option_user_for(queue), key, value)
        {
            // The special request never ran — the queue gate refused it, or it
            // timed out in the queue ([`Self::report_special_never_ran`]). No
            // option was written, and the `setOption -> getOptions` fall-through
            // lives *inside* that request (asynRecord.c:845-849), so the readback
            // does not happen either. Reporting the set failure and then
            // re-reading would claim a driver round-trip the port never made.
            if e.never_ran() {
                return self.report_special_never_ran(&e);
            }
            self.report_error(format!("Error setting option, {}", e.message()));
        }
        // The re-read runs *inside* the request the put queued (C's `setOption`
        // callback falls through to `getOptions`, asynRecord.c:845-849), so it
        // is queued under the same class: a HOSTINFO put that reached a
        // disconnected driver reads the new host:port back off it.
        self.posting(OPTION_READBACK_FIELDS, |this| {
            this.read_options_from_driver(&entry.handle, queue);
        });
        SpecialRan::Yes
    }

    /// Whether an `asynCallbackSpecial` body actually ran, which is what decides
    /// whether C's tail runs — see [`Self::special_callback`].
    ///
    /// C `asynCallbackSpecial` (asynRecord.c:788-900) is a switch whose every arm
    /// falls out to one `monitorStatus` (:897). An arm's error `break` (:862,
    /// :872, :886) leaves the *switch*, not the function, and `setOption`'s "No
    /// asynOption interface" `return` (:1767-1771) leaves `setOption`, not the
    /// callback — so a failed option write, a failed EOS write and a failed
    /// connect all repost status. The only path that does not is a request that
    /// never left the queue: `queueTimeoutCallbackSpecial` (:929-938) is a
    /// different callback entirely and calls no `monitorStatus`.
    ///
    /// Run the body of an `asynCallbackSpecial` and then C's tail: `monitorStatus`
    /// (asynRecord.c:897), POST_IF_NEW'd over [`MONITOR_STATUS_FIELDS`].
    ///
    /// The single owner of that tail. Every special callback goes through it, so
    /// an arm cannot be written that quietly leaves CNCT / ENBL / AUCT / the trace
    /// readbacks stale on the operator's screen — which is what the option, EOS
    /// and CNCT arms each did (W10-D2). Most visibly on a *failed* CNCT put: the
    /// field is snapped back to the wire's real state, and without the tail no
    /// monitor fired, so the screen kept showing the value the operator typed.
    fn special_callback(&mut self, body: impl FnOnce(&mut Self) -> SpecialRan) {
        let before = self.field_snapshot(MONITOR_STATUS_FIELDS);
        if body(self) == SpecialRan::Yes {
            self.monitor_status();
            self.post_if_new(&before);
        }
    }

    /// C `reportError` (asynRecord.c:2028-2049): **the** single owner of every
    /// ERRS write.
    ///
    /// C's shape, and the whole point of it: write the field, then
    /// `db_post_events(errs, DBE_VALUE|DBE_LOG)` iff the text actually changed
    /// (:2044-2048). ERRS is not `pp(TRUE)` (asynRecord.dbd:366-370), and the
    /// record's own writes are not puts, so nothing else posts it — a diagnostic
    /// written straight into the field reached the operator's screen only if some
    /// *other* field happened to be posted for an unrelated reason. Every refusal,
    /// option error, EOS error, connect error and trace-file error was invisible
    /// to a CA client monitoring ERRS (R15-49).
    ///
    /// The change guard is C's, not an optimisation: an unchanged ERRS must not
    /// re-post, or a record retrying a down port every second would fire a monitor
    /// every second with text the client already has.
    fn report_error(&mut self, msg: impl Into<String>) {
        let before = self.field_snapshot(&["ERRS"]);
        self.errs = msg.into();
        self.post_if_new(&before);
    }

    /// C `resetError` (asynRecord.c:2050-2060): clear ERRS and post it if the
    /// operator was looking at a message that is now gone — the same write-then-
    /// post-if-changed shape as [`Self::report_error`], with the empty string.
    ///
    /// The single owner of "the record starts this operation with a clean ERRS".
    /// C calls it at the entry of every operation that can report — `process()`
    /// on an idle record (:339) and again in the callback it queues (:817),
    /// `special()` (:390) and `connectDevice()` (:1151) — never at an exit, so no
    /// diagnostic an operation raised can be cleaned up behind it. Clearing on
    /// entry is what makes ERRS "the last operation's message", not "the last
    /// message any operation left".
    fn reset_error(&mut self) {
        self.report_error(String::new());
    }

    /// C's `state == stateNoDevice` refusal (asynRecord.c:356-357): a record with
    /// no port refuses the transfer in `process()`, before `performIO` — and so
    /// before the interface dispatch — because there is no port to ask about
    /// interfaces. Both the `process()` gate and [`Self::perform_io`] report it
    /// through here so the text has one owner.
    ///
    /// The refusal is an alarm, not just a message: C falls out of that branch
    /// into `recGblSetSevr(pasynRec, STATE_ALARM, MINOR_ALARM)` (asynRecord.c:361)
    /// on *every* such process, so a record whose port never came up sits in
    /// STATE/MINOR rather than NO_ALARM. Staged in `io_alarm` and committed by
    /// `check_alarms` on this cycle, like every other record alarm.
    fn report_not_connected(&mut self) {
        self.report_error("Not connect to a port");
        self.io_alarm = Some((alarm_status::STATE_ALARM, AlarmSeverity::Minor));
    }

    /// C's "the port does not implement this interface" refusal: report and raise
    /// COMM_ALARM/MAJOR_ALARM, do no I/O. C runs it from `performIO`'s four
    /// dispatch arms (asynRecord.c:1328-1360), from `setEos` on `!octetiv`
    /// (:1957-1960) and from `setOption` on `!optioniv` (:1767-1770) — the same
    /// three lines each time, so they are one owner here.
    ///
    /// The severity is staged in `io_alarm` and committed by `check_alarms` on
    /// this cycle, like every other record alarm.
    fn report_no_interface(&mut self, asyn_name: &str) {
        self.report_error(format!("No {asyn_name} interface"));
        self.io_alarm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Major));
    }

    /// C `queueTimeoutCallbackSpecial` (asynRecord.c:929-938): the option / EOS /
    /// connect-readback request waited out `QUEUE_TIMEOUT` and was removed from
    /// the queue. C reports the text, returns the record to `stateIdle` and frees
    /// the request — and, unlike the process timeout, raises **no** severity, so
    /// this must not go through `report_no_interface`'s alarm-raising shape.
    ///
    /// The single owner of that outcome for every request the record's
    /// [`Self::option_user`] builds.
    fn report_special_queue_timeout(&mut self) {
        self.report_error(SPECIAL_QUEUE_TIMEOUT_MSG);
    }

    /// The outcome of a special request that **never ran** — the single owner of
    /// C's two no-callback exits, and of the answer every `asynCallbackSpecial`
    /// arm gives when its request comes back refused or timed out.
    ///
    /// - The port's queue gate refused it: C's `queueRequest` returned
    ///   non-success, so `special()` writes `pasynUser->errorMessage` to ERRS and
    ///   frees the user (asynRecord.c:571-576). `asynCallbackSpecial` never runs
    ///   — no option or EOS is written, no connect is attempted, no readback, and
    ///   no `monitorStatus` tail.
    /// - It waited out `QUEUE_TIMEOUT`: `queueTimeoutCallbackSpecial` runs
    ///   instead of the callback (:929-938), reporting its own text.
    ///
    /// Returns [`SpecialRan::No`] either way, so the caller's `return` is the
    /// whole exit: [`Self::special_callback`] then skips C's tail. An arm that
    /// instead reported the refusal and carried on would drive a readback,
    /// `monitorStatus` and its posts off a request the port never accepted.
    fn report_special_never_ran(&mut self, e: &AsynError) -> SpecialRan {
        if e.is_queue_refused() {
            // C splices `pasynUser->errorMessage` — the gate's own text, e.g.
            // "port X not connected" — into ERRS (:575). No severity: C's
            // `reportError` raises none.
            self.report_error(e.message());
        } else {
            self.report_special_queue_timeout();
        }
        SpecialRan::No
    }

    /// The record's I/O Intr machinery, shared with its `asynRecordDevice`
    /// device support (C reaches the same `asynRecPvt` through `pasynRec->dpvt`).
    pub(crate) fn io_intr_scan(&self) -> Arc<IoIntrScan> {
        self.io_intr.clone()
    }

    /// Publish the record's current interrupt binding — C `registerInterrupts`
    /// reads PORT / IFACE / ADDR / REASON / UI32MASK straight off `pasynRec`
    /// every time it registers (asynRecord.c:599-656), so every put that moves
    /// one of them must reach the registration.
    ///
    /// The single owner of that hand-off: `connect_device` (a new port /
    /// address / drvInfo) and the REASON / IFACE / UI32MASK puts all write
    /// through it, so a live subscription can never describe a binding the
    /// record no longer has. A registration refused by the port ("No asynInt32
    /// interface") lands in ERRS exactly as C's `registerInterrupts` reports it.
    fn publish_io_intr_binding(&mut self) {
        let binding = self.port_entry.as_ref().map(|entry| IoIntrBinding {
            handle: entry.handle.clone(),
            iface: InterfaceType::from_u16(self.iface as u16),
            addr: self.addr,
            reason: self.resolved_reason,
            ui32mask: self.ui32mask,
        });
        if let Err(msg) = self.io_intr.rebind(binding) {
            self.report_error(msg);
        }
    }

    /// C `cancelIOInterruptScan` (asynRecord.c:794-806): a put that invalidates
    /// what the record subscribed to — REASON, IFACE, UI32MASK (:490,:494,:497)
    /// or a PCNCT=0 detach (:525) — takes the record **off** the I/O Intr scan
    /// list rather than silently re-registering behind the operator's back:
    /// `dbPutField(&scanAddr, DBR_LONG, &passiveScan, 1)`. The put is what
    /// cancels the driver registration (`scanDelete` → `get_ioint_info(1)`).
    ///
    /// Both halves run here: the local `set_active(false)` is the registration
    /// (so the driver stops pushing values the instant the field changes, with
    /// no window where a stale subscription can fill the sample cell), and the
    /// SCAN put makes the field itself read back "Passive" as C's does. The put
    /// re-enters `set_io_intr_scan(false)`, which is idempotent.
    ///
    /// C is a no-op when SCAN is not I/O Intr, and so is this.
    fn cancel_io_interrupt_scan(&mut self) {
        if !self.io_intr.is_active() {
            return;
        }
        let _ = self.io_intr.set_active(false);
        let Some((name, db)) = self.async_ctx.clone() else {
            // No database (a record built outside an IOC): the registration is
            // cancelled above; there is no SCAN field to put.
            return;
        };
        // C calls `dbPutField` inline; this record is currently locked by the
        // put that reached `special()`, so the write is handed to the database
        // rather than re-entered here. It lands as soon as this put returns —
        // C's `dbScanLock` is recursive, this one is not.
        let passive = EpicsValue::Enum(ScanType::Passive.to_u16());
        tokio::spawn(async move {
            let _ = db.put_pv(&format!("{name}.SCAN"), passive).await;
        });
    }

    /// C's four `callbackInterrupt*` routines (asynRecord.c:709-792) write the
    /// pushed value into the IFACE's input field. In C that happens on the
    /// driver's thread, under `interruptLock`, before `scanIoRequest`; here the
    /// value rides the sample cell and lands on the process cycle it triggered —
    /// the record is only mutable there. The field written is the same.
    fn apply_io_intr_sample(&mut self, sample: IoIntrSample) {
        match sample {
            IoIntrSample::Octet(s) => self.tinp = s,
            IoIntrSample::Int32(v) => self.i32inp = v,
            IoIntrSample::UInt32(v) => self.ui32inp = v,
            IoIntrSample::Float64(v) => self.f64inp = v,
        }
    }

    /// Does the connected port implement the interface?
    ///
    /// Asked of the port's own registry (`PortHandle::has_interface`, the
    /// `findInterface` of asynRecord.c:1177-1240), not of the record's `*IV`
    /// fields: those are *readbacks* that `connect_device` copies out of the same
    /// registry for the operator to see. Keeping the gate on the registry means a
    /// record cannot end up refusing I/O the port supports (or attempting I/O it
    /// does not) because a readback field was left behind. A record with no port
    /// has no interfaces.
    fn has_interface(&self, iface: InterfaceType) -> bool {
        self.port_has(iface.registry_type())
    }

    /// [`Self::has_interface`] for an interface with no IFACE menu entry —
    /// `asynOption`, `asynGpib`. Same registry, same question.
    fn port_has(&self, iface: crate::interfaces::InterfaceType) -> bool {
        self.port_entry
            .as_ref()
            .is_some_and(|entry| entry.handle.has_interface(iface))
    }

    /// Write one EOS string to the driver, then re-read *both* back — the
    /// `callbackSetEos` → `callbackGetEos` fall-through (asynRecord.c:851-855).
    /// Same invariant as [`Self::write_option`]: IEOS/OEOS show the driver's
    /// EOS, not the requested one, whether or not the set succeeded.
    ///
    /// C `setEos` reports a failing `setOutputEos`/`setInputEos` as
    /// `"Error setting output eos, %s"` / `"Error setting input eos, %s"` with
    /// `pasynUser->errorMessage` (asynRecord.c:1968-1983).
    fn write_eos(&mut self, output: bool) {
        self.special_callback(|this| this.write_eos_body(output));
    }

    /// The `setEos` arm of C's `asynCallbackSpecial` (asynRecord.c:850-854). Its
    /// tail — `monitorStatus` — is [`Self::special_callback`]'s.
    fn write_eos_body(&mut self, output: bool) -> SpecialRan {
        let Some(entry) = self.port_entry.clone() else {
            return SpecialRan::No;
        };
        // C setEos (asynRecord.c:1956-1961). Same as `setOption`'s refusal: the
        // return leaves `setEos`, and the callback's tail still runs.
        if !self.has_interface(InterfaceType::Octet) {
            self.report_no_interface(InterfaceType::Octet.c_asyn_name());
            return SpecialRan::Yes;
        }
        let field = if output { &self.oeos } else { &self.ieos };
        let bytes = translate_escape(field);
        let res = if output {
            entry
                .handle
                .set_output_eos_blocking(self.option_user(), &bytes)
        } else {
            entry
                .handle
                .set_input_eos_blocking(self.option_user(), &bytes)
        };
        if let Err(e) = res {
            // Same rule as `write_option`: a special request that never ran wrote
            // nothing and runs no `getEos` fall-through (asynRecord.c:851-854).
            if e.never_ran() {
                return self.report_special_never_ran(&e);
            }
            let which = if output { "output" } else { "input" };
            self.report_error(format!("Error setting {which} eos, {}", e.message()));
        }
        self.posting(EOS_READBACK_FIELDS, |this| {
            this.read_eos_from_driver(&entry.handle);
        });
        SpecialRan::Yes
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

    /// C `monitorStatus` (asynRecord.c:1042-1140) on the scan thread: re-import
    /// the trace masks and re-read AUCT / CNCT / ENBL from the port, so every
    /// readback field shows the port's current state.
    ///
    /// The single owner of that refresh. C runs it from `connectDevice`
    /// (:1271,:1319), from every completed callback (:897), and from
    /// `exceptCallback` (:913). Here the first two are this function; the
    /// exception path cannot take `&mut self` (the record is owned by the
    /// database), so it does the same work out of band through
    /// [`Self::register_exception_callback`], falling back to `status_dirty` +
    /// this function when the record has no database handle.
    ///
    /// A failing query lands as 0, exactly as C's `else` branches do
    /// (:1087,:1092,:1097).
    fn monitor_status(&mut self) {
        self.read_trace_state();
        let (enabled, auto) = match self.port_entry {
            Some(ref entry) => (
                entry.handle.is_enabled_blocking().unwrap_or(false),
                entry.handle.is_auto_connect_blocking().unwrap_or(false),
            ),
            None => (false, false),
        };
        self.enbl = i32::from(enabled);
        self.auct = i32::from(auto);
        self.refresh_connected_state();
    }

    /// Attempt to connect to the port specified in the PORT field.
    ///
    /// C `connectDevice` (asynRecord.c:1142-1321). The `Err` payload is C's
    /// `pasynUser->errorMessage` — the manager-level text
    /// `pasynManager->connectDevice` leaves on the asynUser
    /// (asynManager.c:1331,1339) — which the caller splices into its own
    /// diagnostic: `special()` reports `"connectDevice failed: %s"` with it
    /// (asynRecord.c:515), while the init and PCNCT paths report nothing further
    /// and leave this function's own `"Connect error, status=%d, %s"`
    /// (asynRecord.c:1158) in ERRS.
    fn connect_device(&mut self) -> Result<(), String> {
        // C `connectDevice` opens with `resetError` (asynRecord.c:1151): the
        // previous connection's diagnostic is cleared *before* the attempt, and
        // whatever this attempt reports (`"Connect error…"`, `"Error in
        // asynDrvUser->create()"`) is what stays in ERRS.
        self.reset_error();

        // C's `REMEMBER_STATE` cache is what its two `monitorStatus` calls
        // (:1270, :1319) post against, and it is seeded before `connectDevice`
        // touches a field — so *everything* the attach changes (the interface
        // flags, REASON, DRVINFO, PCNCT, and the port's ENBL/AUCT/CNCT) is posted,
        // not just the fields `monitorStatus` itself re-reads. The snapshot goes
        // here, above the first assignment, for the same reason (R14-47).
        let before_status = self.field_snapshot(MONITOR_STATUS_FIELDS);

        if self.port.is_empty() {
            self.pcnct = 0;
            self.port_entry = None;
            self.clear_exception_callback();
            // C's failure path is not silent: `bad:` falls into `done:`, which is
            // `cancelIOInterruptScan` + `monitorStatus` (:1310-1319) — the same
            // tail the success path takes, so the operator sees PCNCT and CNCT go
            // to 0 on the record that just lost its port.
            self.monitor_status();
            self.post_if_new(&before_status);
            self.publish_io_intr_binding();
            return Err(self.report_connect_error(
                "asynManager:connectDevice no port name provided".to_string(),
            ));
        }

        match crate::registry::get_port(&self.port) {
            Some(entry) => {
                // C `connectDevice` asks the port for asynDrvUser *before* it
                // resolves anything (`findInterface(asynDrvUserType)`,
                // asynRecord.c:1243). Whether a port can turn a drvInfo string
                // into a reason is a property of the port's interface registry —
                // `PortHandle::has_interface` — not something to infer from a
                // failed `create`.
                if entry
                    .handle
                    .has_interface(crate::interfaces::InterfaceType::DrvUser)
                {
                    // Resolve drvinfo → reason if specified (asynRecord.c:1248-1257).
                    if !self.drvinfo.is_empty() {
                        // Forward the interface this record performs I/O through
                        // (its IFACE field) so an on-demand driver creates the
                        // parameter with the type this record will read it as.
                        let req = crate::port::DrvUserRequest::new(&self.drvinfo, self.addr)
                            .with_iface(InterfaceType::from_u16(self.iface as u16).as_asyn_iface());
                        match entry.handle.drv_user_create_blocking(&req) {
                            Ok(info) => {
                                self.resolved_reason = info.reason;
                                self.reason = info.reason as i32;
                            }
                            Err(_) => {
                                // C reports the bare literal — the driver's own text
                                // reaches the operator through the trace file, not
                                // ERRS (asynRecord.c:1255).
                                self.report_error("Error in asynDrvUser->create()");
                                self.resolved_reason = 0;
                            }
                        }
                    } else {
                        self.resolved_reason = self.reason as usize;
                    }
                } else {
                    // No asynDrvUser — every byte transport (IP, serial, FTDI,
                    // VXI-11). C zeroes REASON unconditionally here
                    // (asynRecord.c:1261): there is no parameter space to point
                    // at, so a REASON the operator restored from a save file must
                    // not survive the connect. A non-blank DRVINFO is a
                    // configuration error, reported and otherwise ignored
                    // (:1263-1265).
                    self.reason = 0;
                    self.resolved_reason = 0;
                    if !self.drvinfo.is_empty() {
                        self.report_error("asynDrvUser not supported but drvInfo not blank");
                    }
                }

                // C `connectDevice` asks the manager for each interface in turn —
                // `findInterface(asynOptionType / asynOctetType / asynInt32Type /
                // asynUInt32DigitalType / asynFloat64Type / asynGpibType)`,
                // asynRecord.c:1177-1240 — and records what it found. A pure-octet
                // transport therefore reads back i32iv/ui32iv/f64iv = 0, and
                // `performIO` refuses I/O on an interface the port does not have
                // (:1328-1360). The port's answer is the driver's own
                // `capabilities()` declaration, taken at registration — see
                // `PortHandle::has_interface`.
                let has = |iface| i32::from(entry.handle.has_interface(iface));
                self.octetiv = has(crate::interfaces::InterfaceType::Octet);
                self.i32iv = has(crate::interfaces::InterfaceType::Int32);
                self.ui32iv = has(crate::interfaces::InterfaceType::UInt32Digital);
                self.f64iv = has(crate::interfaces::InterfaceType::Float64);
                self.optioniv = has(crate::interfaces::InterfaceType::Option);
                self.gpibiv = has(crate::interfaces::InterfaceType::Gpib);

                self.port_entry = Some(entry.clone());

                // Read serial/IP options from driver. C queues this readback at
                // `asynQueuePriorityConnect` carrying
                // `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (asynRecord.c:1277-1280),
                // so a record attaching to a *down* serial or IP port still shows
                // its BAUD/PRTY/HOSTINFO/… instead of "Unknown". C's `getOptions`
                // posts every field it re-read (:1926-1938), so the readback goes
                // in the post bracket.
                self.posting(OPTION_READBACK_FIELDS, |this| {
                    this.read_options_from_driver(&entry.handle, OptionQueue::EvenIfNotConnected);
                });

                // …and the EOS readback C queues right after it, gated on the port
                // having asynOctet (`if(pasynRec->octetiv)`, asynRecord.c:1289-1300)
                // — so a fresh record shows the EOS the *driver* holds instead of a
                // blank IEOS/OEOS until someone happens to write one.
                //
                // C's asymmetry with the option readback above is deliberate and is
                // preserved by the user this runs under: `callbackGetEos` is queued
                // at `asynQueuePriorityLow` and never has its reason overwritten
                // (`duplicateAsynUser` copies it, asynManager.c:1229), so it carries
                // no waiver — on a *disconnected* port the queue gate refuses it
                // (asynManager.c:1547-1552) and IEOS/OEOS stay blank, while the
                // options still fill in.
                // C's `getEos` posts IEOS/OEOS the same way (:2016-2024).
                self.posting(EOS_READBACK_FIELDS, |this| {
                    this.read_eos_from_driver(&entry.handle);
                });

                // Attached to the port (C `connectDevice` sets PCNCT=1,
                // asynRecord.c:1305).
                self.pcnct = 1;
                // C `connectDevice` ends in `monitorStatus` (asynRecord.c:1271,
                // :1319): the trace masks and the port's actual enable /
                // auto-connect / connect state are *queried* into TMSK…/ENBL /
                // AUCT / CNCT. They must not be forced to 1, which would discard
                // a user who configured ENBL=0 or a port registered
                // noAutoConnect, and would claim a live wire on a port whose
                // transport is down. `monitorStatus` posts what it re-imported,
                // and `before_status` (taken at the top) is the `old` cache it
                // posts against.
                self.monitor_status();
                self.post_if_new(&before_status);
                // C `connectDevice` adds `exceptCallback` (asynRecord.c:1269) so
                // any later exception on the port — a trace change, a dropped
                // link, an enable — refreshes the readback fields out of band.
                // Registered after `monitor_status` so the callback's
                // POST_IF_NEW cache is seeded with the values C's `old` holds at
                // the same point.
                self.register_exception_callback();
                // The record now points at a different port / address / reason.
                // C leaves an I/O Intr registration made against the *previous*
                // device in place (`connectDevice` calls no `cancelInterrupts`,
                // asynRecord.c:1142-1321), which keeps firing the old port's
                // values into the record. Re-point it instead: the registration
                // follows the binding, which is the invariant `IoIntrScan` holds.
                // A record not on I/O Intr registers nothing either way.
                self.publish_io_intr_binding();
                Ok(())
            }
            None => {
                self.pcnct = 0;
                self.port_entry = None;
                self.clear_exception_callback();
                // C's `bad:` → `done:` tail, as above (:1310-1319).
                self.monitor_status();
                self.post_if_new(&before_status);
                self.publish_io_intr_binding();
                Err(self.report_connect_error(format!(
                    "asynManager:connectDevice port {} not found",
                    self.port
                )))
            }
        }
    }

    /// C `connectDevice`'s own failure diagnostic (asynRecord.c:1157-1159):
    /// `reportError(status, "Connect error, status=%d, %s", status,
    /// pasynUser->errorMessage)`, where `status` is the numeric `asynStatus`
    /// the manager returned (`asynError` = 3 for every `connectDevice` failure,
    /// asynManager.c:1331-1345). Returns the manager-level message so the
    /// caller can splice it into its own text.
    fn report_connect_error(&mut self, manager_message: String) -> String {
        self.report_error(format!(
            "Connect error, status={}, {manager_message}",
            AsynStatus::Error as i32
        ));
        manager_message
    }

    /// The I/O timeout this cycle carries — the single owner of TMOT's
    /// translation into `AsynUser::timeout`.
    ///
    /// C `asynCallbackProcess` assigns the field verbatim:
    /// `pasynUser->timeout = pasynRec->tmot` (asynRecord.c:818). The `double`
    /// is three-valued, and the transports read all three:
    ///
    /// * `> 0` — bounded wait.
    /// * `== 0` — non-blocking poll. `drvAsynIPPort.c:741-742` computes
    ///   `readPollmsec = (int)(timeout * 1000)` and floors a zero to a 1 ms
    ///   poll; `drvAsynSerialPort.c:902-905` sets `VMIN=0, VTIME=0`, the termios
    ///   "return whatever is already buffered" read.
    /// * `< 0` — wait forever (`readPollmsec = -1`).
    ///
    /// The port used to substitute a 1 s wait for BOTH non-positive cases, so an
    /// operator asking for a poll got a one-second block on a silent device.
    /// `tmot >= 0` now passes through verbatim: `Duration::ZERO` reaches the
    /// transports, whose own zero handling already reproduces C's
    /// (`ip_port::socket_poll_timeout` floors it to the same 1 ms;
    /// `serial_port::duration_to_poll_ms` yields the same 0 ms `poll()`).
    ///
    /// `tmot < 0` cannot be expressed. `AsynUser::timeout` is an unsigned
    /// `Duration` by deliberate framework-wide design — every blocking driver
    /// operation is bounded, so a stuck device cannot wedge the port actor
    /// thread indefinitely. That is the signed-off deviation **DRV-42**
    /// (`user.rs:15-22`, filed at
    /// `doc/c-parity-review-drivers-2026-06-29.md:98`), and this record is not
    /// allowed to override it: a negative TMOT falls back to the bounded
    /// [`crate::user::DEFAULT_TIMEOUT`]. A non-finite or out-of-range TMOT takes
    /// the same fallback — C's `(int)` cast of such a value is undefined
    /// behaviour, so there is no C semantics to port.
    ///
    /// The substitution itself belongs to [`crate::user::timeout_from_secs`],
    /// the crate-wide owner of `f64 seconds` → `AsynUser::timeout`; this method
    /// only names TMOT as the source.
    fn io_timeout(&self) -> std::time::Duration {
        crate::user::timeout_from_secs(self.tmot)
    }

    /// The `asynUser` an option put runs under.
    ///
    /// C queues the option callback on a `duplicateAsynUser` of the record's own
    /// `pasynUser` (asynRecord.c:531-533), and `duplicateAsynUser` copies
    /// `timeout` (asynManager.c:1225). The record's `pasynUser->timeout` is TMOT
    /// (asynRecord.c:818), so an RFC 2217 negotiation triggered by a BAUD/PRTY/…
    /// put is bounded by TMOT — the record's own timeout, not a constant private
    /// to the COM layer. ADDR rides along because C's option callback runs on the
    /// device the record is connected to.
    ///
    /// (C assigns `pasynUser->timeout = tmot` inside `asynCallbackProcess`, so an
    /// option put made before the record has ever processed inherits the 1 s
    /// `createAsynUser` default instead. That lazy assignment is not modelled:
    /// TMOT is the record's timeout from the first put onwards.)
    fn option_user(&self) -> AsynUser {
        AsynUser::new(self.resolved_reason)
            .with_addr(self.addr)
            .with_timeout(self.io_timeout())
            // C `special()` queues the option callback with `QUEUE_TIMEOUT`
            // (asynRecord.c:571-572), and `connectDevice` queues its
            // getOption/getEos readbacks with the same (:1281,:1297) — every
            // request built from this user is one of those.
            .with_queue_timeout(QUEUE_TIMEOUT)
    }

    /// The `asynUser` an option request runs under, for the [`OptionQueue`] class
    /// it belongs to — the single place the record decides whether an option
    /// request may be queued on a disconnected port.
    fn option_user_for(&self, queue: OptionQueue) -> AsynUser {
        match queue {
            OptionQueue::Normal => self.option_user(),
            OptionQueue::EvenIfNotConnected => self.option_user().queue_even_if_not_connected(),
        }
    }

    /// Clamp the operator's requested transfer sizes into the record fields —
    /// the single owner of NOWT/NRRD's effective value.
    ///
    /// C `performOctetIO` does not clamp into locals: it writes the clamped
    /// value back into the record itself — `nowt = omax` when a Binary write
    /// asks for more than the output buffer holds (asynRecord.c:1499) and
    /// `nrrd = inlen` when the read asks for more than the IFMT-selected input
    /// buffer holds (:1512) — and `monitor()` `POST_IF_NEW`s both (:1020,1022).
    /// The fields therefore show the *effective* transfer sizes, not the
    /// requested ones. Called from [`Self::build_io_plan`] on the scan thread
    /// before the plan snapshot, so the synchronous and off-thread runners see
    /// one already-clamped value and nothing downstream re-derives it.
    ///
    /// Gated on the octet interface exactly as C is — `performIO` only reaches
    /// `performOctetIO` for `IFACE == asynOctet` (asynRecord.c:1326-1331), and
    /// the register handlers never touch NRRD/NOWT. Inside `performOctetIO`
    /// both clamps run before any TMOD test, so a Read-only cycle still clamps
    /// NOWT and a Write-only cycle still clamps NRRD.
    ///
    /// C compares with `>` against signed ints, so a negative NRRD/NOWT is left
    /// in the field untouched; mirrored here. The `max(0)` guards at the buffer
    /// arithmetic keep the negative out of the slice math without rewriting the
    /// field.
    /// The one owner of a client put into an array field — C's `put_array_info`
    /// (asynRecord.c:983-993), which `dbPut` calls for every `SPC_DBADDR` field
    /// it writes (`dbAccess.c:1366-1369`) with `nNew` = the element count that
    /// arrived:
    ///
    /// ```c
    /// if (fieldIndex == asynRecordBOUT)      pasynRec->nowt = nNew;
    /// else if (fieldIndex == asynRecordBINP) pasynRec->nord = nNew;
    /// ```
    ///
    /// In C, **writing the array *is* how the count gets set** — NOWT is not an
    /// independent field a client is expected to put alongside BOUT, and
    /// `monitor()` posts it right after (`POST_IF_NEW(nowt)`, :1022). Making the
    /// buffer and its count move together in one method is what keeps them from
    /// diverging: a `caput -a` of 120 bytes into BOUT used to leave NOWT at its
    /// stale value, and [`Self::octet_output_buffer`] then sent `bout[..NOWT]`
    /// — 80 bytes on the wire where C sends 120.
    fn put_array_field(&mut self, field: AsynArrayField, data: Vec<u8>) {
        let n_new = data.len().min(i32::MAX as usize) as i32;
        match field {
            AsynArrayField::Bout => {
                self.bout = data;
                self.nowt = n_new;
            }
            AsynArrayField::Binp => {
                self.binp = data;
                self.nord = n_new;
            }
        }
    }

    fn clamp_transfer_sizes(&mut self, in_len: usize) {
        if self.ofmt == ASYN_FMT_BINARY && self.nowt > self.omax {
            self.nowt = self.omax;
        }
        let in_len = in_len.min(i32::MAX as usize) as i32;
        if self.nrrd > in_len {
            self.nrrd = in_len;
        }
    }

    /// Build the octet output payload by `OFMT`, mirroring
    /// `asynRecord.c:1486-1502`:
    ///   - ASCII  -> `dbTranslateEscape(AOUT)`: the AOUT string with escape
    ///     sequences (`\r\n` -> CRLF) translated.
    ///   - Hybrid -> `dbTranslateEscape(BOUT read as a C string)`: the binary
    ///     output buffer, escape-translated (stops at the first NUL).
    ///   - Binary -> raw BOUT, `NOWT` bytes (already clamped to `OMAX` by
    ///     [`Self::clamp_transfer_sizes`]), no translation.
    ///
    /// ASCII/Hybrid emit the full translated buffer; only Binary is
    /// length-bounded by `NOWT`. Previously the record sent raw AOUT
    /// for both ASCII and Hybrid (ASCII shipped literal backslashes, Hybrid
    /// ignored BOUT) and clamped every mode by `NOWT`.
    fn octet_output_buffer(&self) -> Vec<u8> {
        match self.ofmt {
            ASYN_FMT_BINARY => {
                let nowt = self.nowt.max(0) as usize;
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
    fn build_io_plan(&mut self) -> IoPlan {
        self.build_io_plan_for(None)
    }

    /// [`Self::build_io_plan`] for a cycle that may be a GPIB command instead of
    /// a transfer. A GPIB cycle skips the octet output build and the NRRD/NOWT
    /// clamps: C reaches those only inside `performOctetIO`
    /// (asynRecord.c:1503-1546), which `gpibUniversalCmd` / `gpibAddressedCmd`
    /// replace — a UCMD put must not rewrite the record's transfer sizes.
    fn build_io_plan_for(&mut self, gpib: Option<GpibCycle>) -> IoPlan {
        let iface = InterfaceType::from_u16(self.iface as u16);
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
        // C reaches the NOWT/NRRD clamps and the OFMT output-buffer build only
        // inside `performOctetIO`, i.e. only for the octet interface
        // (asynRecord.c:1326-1331). Gate both here for the same reason: a
        // register-interface cycle must leave NRRD/NOWT alone and has no octet
        // payload to send.
        let (octet_out, octet_out_len) = if gpib.is_none() && iface == InterfaceType::Octet {
            self.clamp_transfer_sizes(in_len);
            let out = self.octet_output_buffer();
            let len = out.len();
            (out, len)
        } else {
            (Vec::new(), 0)
        };
        // NRRD is already clamped to `in_len` for the octet interface — the only
        // one that reads this field — so `min` is the same bound.
        let octet_buf_size = if self.nrrd > 0 {
            (self.nrrd as usize).min(in_len)
        } else {
            in_len
        };
        let timeout = self.io_timeout();
        IoPlan {
            tmod: TransferMode::from_u16(self.tmod as u16),
            iface,
            gpib,
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

    /// Decode and consume this cycle's GPIB command — C `asynCallbackProcess`'s
    /// `ucmd != gpibUCMD_None` / `acmd != gpibACMD_None` test, and the reset back
    /// to `None` it performs right after the call returns
    /// (asynRecord.c:819-827). UCMD wins over ACMD, as in C.
    ///
    /// The reset is unconditional in C — it happens whether the command reached
    /// the bus, failed in the driver, or was refused because the port carries no
    /// asynGpib interface — so an operator's UCMD/ACMD put runs exactly one
    /// cycle. Consuming the field here, above the interface gate, keeps that.
    fn take_gpib_cycle(&mut self) -> Option<GpibCycle> {
        use crate::interfaces::gpib::{
            GpibAddressedRequest, addressed_request, universal_cmd_byte,
        };
        if self.ucmd != 0 {
            let cmd = universal_cmd_byte(self.ucmd);
            self.ucmd = 0;
            return Some(GpibCycle::Universal(cmd));
        }
        if self.acmd != 0 {
            let request = addressed_request(self.acmd, self.addr);
            self.acmd = 0;
            return Some(match request {
                GpibAddressedRequest::Frame(frame) => GpibCycle::Addressed(frame),
                GpibAddressedRequest::SerialPoll => GpibCycle::SerialPoll,
            });
        }
        None
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
        if let Some(v) = out.spr {
            self.spr = v;
        }
        if let Some(v) = out.errs {
            self.report_error(v);
        }
        // The I/O cycle's record alarm (C `recGblSetSevr` in `performIO`).
        // `check_alarms` — invoked by the framework on this same completion
        // cycle (sync `process()` return or async re-entry) — is the sole
        // consumer; it `take()`s and commits it via `rec_gbl_set_sevr`.
        if let Some(a) = out.alarm {
            self.io_alarm = Some(a);
        }
    }

    /// Run one cycle's phases inline — the C `process()` `canBlock==0` branch
    /// (asynRecord.c:351-352), which runs the queued callback's work on the scan
    /// thread rather than queuing it. The cycle is whatever `plan` says: a
    /// `performIO` transfer, or the GPIB command a UCMD/ACMD put asked for.
    /// Shares the op builders and result recorders with the off-thread
    /// [`run_io_plan`]; only the submit primitive differs (blocking here,
    /// awaited there).
    fn perform_io(&mut self, plan: IoPlan) -> CaResult<()> {
        let entry = match &self.port_entry {
            Some(e) => e.clone(),
            None => {
                self.report_not_connected();
                return Ok(());
            }
        };
        let mut out = IoOutcome::default();

        for phase in io_phases(&plan) {
            let res = entry
                .handle
                .submit_blocking(io_phase_op(&plan, phase), io_phase_user(&plan, phase));
            if let PhaseFlow::Aborted = record_phase_result(&plan, &mut out, phase, res) {
                break;
            }
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
        plan: IoPlan,
    ) -> ProcessOutcome {
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
            if let Some(token) = db.mint_async_token(&name) {
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

    /// `asynRecord.c:84-91` — the rset NULLs every property slot except
    /// `get_precision`:
    ///
    /// ```c
    /// #define get_units NULL
    /// static long get_precision(const struct dbAddr * paddr, long *precision);
    /// #define get_enum_str NULL
    /// #define get_enum_strs NULL
    /// #define get_graphic_double NULL
    /// #define get_control_double NULL
    /// #define get_alarm_double NULL
    /// ```
    ///
    /// So `dbGet` clears `DBR_UNITS`, `DBR_GR_DOUBLE`, `DBR_CTRL_DOUBLE`,
    /// `DBR_AL_DOUBLE` and `DBR_ENUM_STRS` for every asyn field, and QSRV2
    /// marks none of those leaves. Without this row the record fell to
    /// `default_property_support`'s untranscribed `_ => NUMERIC` arm and the
    /// port marked all six on every field — a fabricated `display.units` of
    /// `""` and `valueAlarm` bands of zero, presented as authoritative.
    ///
    /// This row lives here, not in `default_property_support`'s table,
    /// because asyn-rs owns `asynRecord`: a downstream crate cannot add a row
    /// to a table inside epics-base-rs. That is what
    /// [`Record::property_support`] being a trait method buys.
    fn property_support(&self) -> epics_base_rs::server::snapshot::PropertySupport {
        use epics_base_rs::server::snapshot::PropertySupport as P;
        P {
            precision: true,
            ..P::NONE
        }
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
            "PCNCT" => Some(EpicsValue::Enum(self.pcnct as u16)),
            "DRVINFO" => Some(EpicsValue::String(self.drvinfo.clone().into())),
            "REASON" => Some(EpicsValue::Long(self.reason)),
            "TMOD" => Some(EpicsValue::Enum(self.tmod as u16)),
            "TMOT" => Some(EpicsValue::Double(self.tmot)),
            "IFACE" => Some(EpicsValue::Enum(self.iface as u16)),
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
            "OFMT" => Some(EpicsValue::Enum(self.ofmt as u16)),
            "AINP" => Some(EpicsValue::String(self.ainp.clone().into())),
            "TINP" => Some(EpicsValue::String(self.tinp.clone().into())),
            "IEOS" => Some(EpicsValue::String(self.ieos.clone().into())),
            "BINP" => Some(EpicsValue::CharArray(self.binp.clone())),
            "IMAX" => Some(EpicsValue::Long(self.imax)),
            "NRRD" => Some(EpicsValue::Long(self.nrrd)),
            "NORD" => Some(EpicsValue::Long(self.nord)),
            "IFMT" => Some(EpicsValue::Enum(self.ifmt as u16)),
            "EOMR" => Some(EpicsValue::Enum(self.eomr as u16)),
            "I32INP" => Some(EpicsValue::Long(self.i32inp)),
            "I32OUT" => Some(EpicsValue::Long(self.i32out)),
            "UI32INP" => Some(EpicsValue::ULong(self.ui32inp)),
            "UI32OUT" => Some(EpicsValue::ULong(self.ui32out)),
            "UI32MASK" => Some(EpicsValue::ULong(self.ui32mask)),
            "F64INP" => Some(EpicsValue::Double(self.f64inp)),
            "F64OUT" => Some(EpicsValue::Double(self.f64out)),
            "BAUD" => Some(EpicsValue::Enum(self.baud as u16)),
            "LBAUD" => Some(EpicsValue::Long(self.lbaud)),
            "PRTY" => Some(EpicsValue::Enum(self.prty as u16)),
            "DBIT" => Some(EpicsValue::Enum(self.dbit as u16)),
            "SBIT" => Some(EpicsValue::Enum(self.sbit as u16)),
            "MCTL" => Some(EpicsValue::Enum(self.mctl as u16)),
            "FCTL" => Some(EpicsValue::Enum(self.fctl as u16)),
            "IXON" => Some(EpicsValue::Enum(self.ixon as u16)),
            "IXOFF" => Some(EpicsValue::Enum(self.ixoff as u16)),
            "IXANY" => Some(EpicsValue::Enum(self.ixany as u16)),
            "HOSTINFO" => Some(EpicsValue::String(self.hostinfo.clone().into())),
            "DRTO" => Some(EpicsValue::Enum(self.drto as u16)),
            "UCMD" => Some(EpicsValue::Enum(self.ucmd as u16)),
            "ACMD" => Some(EpicsValue::Enum(self.acmd as u16)),
            "SPR" => Some(EpicsValue::Char(self.spr as u8)),
            "TMSK" => Some(EpicsValue::Long(self.tmsk)),
            "TB0" => Some(EpicsValue::Enum(self.tb0 as u16)),
            "TB1" => Some(EpicsValue::Enum(self.tb1 as u16)),
            "TB2" => Some(EpicsValue::Enum(self.tb2 as u16)),
            "TB3" => Some(EpicsValue::Enum(self.tb3 as u16)),
            "TB4" => Some(EpicsValue::Enum(self.tb4 as u16)),
            "TB5" => Some(EpicsValue::Enum(self.tb5 as u16)),
            "TIOM" => Some(EpicsValue::Long(self.tiom)),
            "TIB0" => Some(EpicsValue::Enum(self.tib0 as u16)),
            "TIB1" => Some(EpicsValue::Enum(self.tib1 as u16)),
            "TIB2" => Some(EpicsValue::Enum(self.tib2 as u16)),
            "TINM" => Some(EpicsValue::Long(self.tinm)),
            "TINB0" => Some(EpicsValue::Enum(self.tinb0 as u16)),
            "TINB1" => Some(EpicsValue::Enum(self.tinb1 as u16)),
            "TINB2" => Some(EpicsValue::Enum(self.tinb2 as u16)),
            "TINB3" => Some(EpicsValue::Enum(self.tinb3 as u16)),
            "TSIZ" => Some(EpicsValue::Long(self.tsiz)),
            "TFIL" => Some(EpicsValue::String(self.tfil.clone().into())),
            "AUCT" => Some(EpicsValue::Enum(self.auct as u16)),
            "CNCT" => Some(EpicsValue::Enum(self.cnct as u16)),
            "ENBL" => Some(EpicsValue::Enum(self.enbl as u16)),
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
                self.put_array_field(AsynArrayField::Bout, to_bytes(&value));
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
                self.put_array_field(AsynArrayField::Binp, to_bytes(&value));
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
            // C init_record (asynRecord.c:281-283) ignores the returned status
            // beyond the state it sets; `connectDevice`'s own "Connect error…"
            // text is what stays in ERRS.
            let _ = self.connect_device();
        }
        Ok(())
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        // C reaches `special()` only for the fields the dbd marks
        // `special(SPC_MOD)`; this port's framework calls it after *every*
        // accepted put, so [`SPC_MOD_FIELDS`] is the gate that restores C's set.
        // Without it the entry `resetError` below would fire on a put to a plain
        // field — VAL, NRRD, TMOT, or ERRS itself — and wipe a diagnostic C keeps.
        if !SPC_MOD_FIELDS.contains(&field) {
            return Ok(());
        }
        // C `special()` opens with `resetError` (asynRecord.c:390), before any
        // field dispatch: whatever this put reports is the only thing in ERRS
        // when it returns.
        self.reset_error();

        match field {
            // Connection fields → reconnect. C overwrites `connectDevice`'s own
            // diagnostic with its own on failure (asynRecord.c:514-516):
            // "connectDevice failed: <pasynUser->errorMessage>".
            "PORT" | "ADDR" | "DRVINFO" => {
                if let Err(manager_message) = self.connect_device() {
                    self.report_error(format!("connectDevice failed: {manager_message}"));
                }
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
            // CNCT — the `callbackConnect` arm (asynRecord.c:857-889). Its tail is
            // C's own: `monitorStatus` re-reads CNCT from `isConnected`
            // (:1089-1093), so a refused or failed request snaps the field back to
            // the wire's real state — *and posts it*, which is the half that was
            // missing (W10-D2). Without the post the snap-back was invisible: the
            // operator's screen kept showing the value they typed at a port that
            // had refused it.
            "CNCT" => self.special_callback(|this| {
                let want = this.cnct != 0;
                match this.port_entry {
                    Some(ref entry) => {
                        let handle = entry.handle.clone();
                        // The special user is the record's own — connected at
                        // the record's ADDR (C `duplicateAsynUser` of a user
                        // `connectDevice`d at pasynRec->addr). No sentinel, so
                        // on a disconnected port a device-addressed (ADDR>=0)
                        // CNCT put is refused `asynDisconnected` exactly as
                        // C's `checkPortConnect` refuses it (W10-D1); the
                        // refusal lands in ERRS and monitorStatus snaps CNCT
                        // back below.
                        let cnct_user = || AsynUser::new(0).with_addr(this.addr);
                        let res = match (want, handle.is_connected_blocking()) {
                            (true, Ok(false)) => Some((
                                "connect",
                                handle
                                    .submit_blocking(RequestOp::Connect, cnct_user())
                                    .map(|_| ()),
                            )),
                            (false, Ok(true)) => Some((
                                "disconnect",
                                handle
                                    .submit_blocking(RequestOp::Disconnect, cnct_user())
                                    .map(|_| ()),
                            )),
                            // Already in the requested state: C issues
                            // no driver call at all.
                            (_, Ok(_)) => None,
                            (_, Err(e)) => {
                                this.report_error(format!(
                                    "asynCallbackSpecial isConnected error: {e}"
                                ));
                                None
                            }
                        };
                        if let Some((what, Err(e))) = res {
                            // The gate refused the request, or it timed out in the
                            // queue: `asynCallbackSpecial` never ran, so C's
                            // `special()` reports the refusal and frees the user
                            // (asynRecord.c:571-576) — no connect was attempted and
                            // there is no callback tail to run. This is the arm the
                            // waiver rules bite on: a device-addressed CNCT put to a
                            // disconnected port is refused `asynDisconnected`
                            // (W10-D1).
                            if e.never_ran() {
                                return this.report_special_never_ran(&e);
                            }
                            this.report_error(format!(
                                "asynCallbackSpecial callbackConnect {what}: {e}"
                            ));
                        }
                    }
                    None => {
                        this.report_error("asynCallbackSpecial isConnected error");
                    }
                }
                // Every one of those paths — including each error `break` — falls
                // out of C's switch into `monitorStatus` (:897).
                SpecialRan::Yes
            }),

            // PCNCT — attach / detach *this record* to the port. C
            // asynRecord.c:519-527: `connectDevice` on 1;
            // `exceptionCallbackRemove` + `pasynManager->disconnect` +
            // `cancelIOInterruptScan` on 0. The driver's transport is not
            // touched either way.
            "PCNCT" => {
                if self.pcnct != 0 {
                    // C asynRecord.c:520-521 keeps `connectDevice`'s own
                    // "Connect error…" text; it adds no wrapper of its own.
                    let _ = self.connect_device();
                } else {
                    // C asynRecord.c:522-526, in this order:
                    // exceptionCallbackRemove, disconnect, cancelIOInterruptScan.
                    self.port_entry = None;
                    self.clear_exception_callback();
                    // Detached: `isConnected` has no device to report on, which
                    // is C's CNCT=0 (monitorStatus, :1091-1093). The refresh goes
                    // in the post bracket like every other one — a field that
                    // moves without its monitor is a field the operator's screen
                    // never learns about (R14-47).
                    self.posting(&["CNCT"], |this| this.refresh_connected_state());
                    self.cancel_io_interrupt_scan();
                    self.publish_io_intr_binding();
                }
            }

            // Interface change. C `special()` case asynRecordIFACE
            // (asynRecord.c:493-495) does exactly one thing:
            // `cancelIOInterruptScan`. The transfer path needs no notice — the
            // field is read by `performIO` on the next process, where the port's
            // interface registry decides whether the transfer runs at all
            // (:1328-1360, `has_interface`) — but an I/O Intr registration was
            // made against the *old* interface and must not survive the change.
            "IFACE" => {
                self.cancel_io_interrupt_scan();
                self.publish_io_intr_binding();
            }

            // REASON change. C `special()` case asynRecordREASON
            // (asynRecord.c:487-492) runs four statements, and the port ran two.
            //
            // `strcpy(pasynRec->drvinfo, "")` (:489) is not cosmetic — it is what
            // makes the operator's put *stick*. DRVINFO is the other way to set
            // REASON: `connectDevice` re-resolves it through
            // `asynDrvUser->create` and assigns the result over REASON whenever
            // DRVINFO is non-empty (:1248-1254). Leaving the old DRVINFO in place
            // meant the next reconnect — a PORT/ADDR put, a PCNCT=1, an IOC
            // restart with a saved DRVINFO — silently re-resolved REASON from a
            // string the operator had just overridden by hand.
            //
            // `monitorStatus` (:491) is the only place C posts DRVINFO
            // (:1129-1132), so without it the blank never reaches the operator's
            // screen; it also re-imports the trace / enable / auto-connect /
            // connect readbacks, which is why the whole [`MONITOR_STATUS_FIELDS`]
            // set is what gets POST_IF_NEW'd here and not just DRVINFO.
            "REASON" => self.special_callback(|this| {
                this.resolved_reason = this.reason as usize;
                this.drvinfo.clear();
                this.cancel_io_interrupt_scan();
                this.publish_io_intr_binding();
                // C calls `monitorStatus` from `special()` itself here (:491)
                // rather than from a callback tail, but it is the same statement
                // over the same field set — so it is the same owner.
                SpecialRan::Yes
            }),

            // --- Serial options ---
            //
            // Every arm dispatches: C `setOption` has a `case` per field and no
            // value gate anywhere (asynRecord.c:1777-1826), so *whatever* the
            // operator put — including the index-0 "Unknown" of each menu, an
            // LBAUD of 0, an empty HOSTINFO — reaches the driver, and the
            // `/* no break */` fall-through (:845-849) then refreshes every
            // option readback from the driver. An arm that returns early instead
            // skips both halves: the driver never sees the write, and the field
            // is left showing a value the port never took.
            //
            // The menu → choice text mapping is C's own choice arrays; see
            // [`menu_choice`].
            "BAUD" => {
                let val = menu_choice(BAUD_CHOICES, self.baud);
                self.write_option("baud", val);
            }
            "LBAUD" => {
                // C `sprintf(optionString, "%d", pasynRec->lbaud)` (:1783-1785):
                // the long baud is sent verbatim, not through a menu.
                self.write_option("baud", &self.lbaud.to_string());
            }
            "PRTY" => {
                let val = menu_choice(PARITY_CHOICES, self.prty);
                self.write_option("parity", val);
            }
            "DBIT" => {
                // C parity: `drvAsynSerialPort.c:146/360` recognises the key
                // `"bits"` (not `"csize"`), and so does this port's serial driver
                // (`serial_port.rs:649`).
                let val = menu_choice(DBIT_CHOICES, self.dbit);
                self.write_option("bits", val);
            }
            "SBIT" => {
                let val = menu_choice(SBIT_CHOICES, self.sbit);
                self.write_option("stop", val);
            }
            "MCTL" => {
                let val = menu_choice(MCTL_CHOICES, self.mctl);
                self.write_option("clocal", val);
            }
            "FCTL" => {
                let val = menu_choice(FCTL_CHOICES, self.fctl);
                self.write_option("crtscts", val);
            }
            "IXON" => {
                let val = menu_choice(IX_CHOICES, self.ixon);
                self.write_option("ixon", val);
            }
            "IXOFF" => {
                let val = menu_choice(IX_CHOICES, self.ixoff);
                self.write_option("ixoff", val);
            }
            "IXANY" => {
                let val = menu_choice(IX_CHOICES, self.ixany);
                self.write_option("ixany", val);
            }

            // --- IP options ---
            "HOSTINFO" => {
                self.write_option("hostinfo", &self.hostinfo.clone());
            }
            "DRTO" => {
                let val = menu_choice(DRTO_CHOICES, self.drto);
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
            // The C `wasQueued` branch's STATE_ALARM/MAJOR_ALARM (:399) rides on
            // that same outcome: `IoOutcome::report_canceled` owns both halves of
            // the cancel event, so the re-entry commits the alarm with the message.
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
            // Both puts run the C set→get fall-through through the single
            // `write_eos` owner (asynRecord.c:851-855).
            "OEOS" => self.write_eos(true),
            "IEOS" => self.write_eos(false),

            // --- UI32MASK change ---
            //
            // C `special()` case asynRecordUI32MASK (asynRecord.c:496-498):
            // `cancelIOInterruptScan`. The mask is a *registration* parameter for
            // asynUInt32Digital (`registerInterruptUser(..., ui32mask, ...)`,
            // :635) as well as a per-transfer one, so an existing registration
            // still carries the old mask and must go.
            "UI32MASK" => {
                self.cancel_io_interrupt_scan();
                self.publish_io_intr_binding();
            }

            _ => {}
        }
        Ok(())
    }

    /// C `process()` (asynRecord.c:329-372). The body is `Self::process_cycle`;
    /// this wrapper is C's `done:` label (:369-371), whose `gotValue = 0` is the
    /// invariant that keeps the interrupt cell honest:
    ///
    /// > **An interrupt sample MUST NOT outlive the process cycle it was visible
    /// > to.** The only cycle that may leave it set is one that returns with
    /// > `pact = TRUE` — C returns from *inside* the `stateIdle` arm (:349-351)
    /// > and never reaches `done:`.
    ///
    /// Every other arm — the completion re-entry, the `stateNoDevice` refusal,
    /// the interrupt-driven cycle itself, the inline `performIO`, a failed
    /// `queueRequest` — falls into `done:` and clears the flag. Enforcing that
    /// here, at the single exit, is what makes it hold by construction: no arm
    /// of `process_cycle` can forget to discard a sample it did not consume.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let outcome = self.process_cycle();
        let went_async = matches!(
            &outcome,
            Ok(o) if matches!(
                o.result,
                RecordProcessResult::AsyncPending | RecordProcessResult::AsyncPendingNotify(_)
            )
        );
        if !went_async {
            self.io_intr.clear_sample();
        }
        outcome
    }

    /// C `getIoIntInfo` (asynRecord.c:582-597), reached from `dbScan`'s
    /// `scanAdd` / `scanDelete`: register the driver interrupt callbacks when
    /// the record joins the I/O Intr scan list, cancel them when it leaves.
    ///
    /// `registerInterrupts` failing is C's `return -1`, which makes `scanAdd`
    /// report the error and leave the record Passive (dbScan.c:278-293). The
    /// port's `setup_io_intr` runs the same demotion off
    /// `AsynRecordDevice::io_intr_receiver`; for a *runtime* SCAN put
    /// the failure text is what reaches the operator, in ERRS, exactly as C's
    /// `reportError` puts it there (:617,:627,:637,:647).
    fn set_io_intr_scan(&mut self, active: bool) {
        if let Err(msg) = self.io_intr.set_active(active) {
            self.report_error(msg);
        }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn clears_udf(&self) -> bool {
        true
    }

    /// C `asynRecord.c` init_record pass 0: `pasynRec->udf = 0;
    /// recGblResetAlarms(pasynRec)` — set unconditionally, before any connect,
    /// so a freshly loaded asyn record is defined and no-alarm even against a
    /// disconnected port. Without this the record shows the born
    /// UDF/INVALID/UDF (a device-config record has no undefined-value state to
    /// justify it). The init owner performs the reset — see
    /// [`Record::init_resets_alarms`].
    fn init_resets_alarms(&self) -> bool {
        true
    }

    /// The `SPC_DBADDR` octet buffers: C `cvt_dbaddr` (asynRecord.c:948-955)
    /// fixes the channel's `no_elements = omax` (BOUT) / `imax` (BINP) — the
    /// buffer capacity, `80` by default — while `get_array_info` reports the
    /// current transferred length (`nowt`/`nord`). So `ca_element_count` is the
    /// capacity even though `get_field` serves the shorter transferred bytes.
    fn field_native_count(&self, field: &str) -> Option<u32> {
        match field {
            "BOUT" => Some(self.omax.max(0) as u32),
            "BINP" => Some(self.imax.max(0) as u32),
            _ => None,
        }
    }
}

impl AsynRecord {
    /// The body of C's `process()` — every arm of its `state` chain. The
    /// `done:` epilogue that closes the interrupt-cell invariant lives in the
    /// [`Record::process`] wrapper that calls this.
    fn process_cycle(&mut self) -> CaResult<ProcessOutcome> {
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

        // Re-run the C `monitorStatus` refresh if any exception fired on the
        // port since the last cycle — an external `setTrace*`, a dropped link,
        // an enable / auto-connect change. C refreshes these readback fields
        // from its `exceptCallback` immediately (asynRecord.c:903-917). When the
        // record carries a database handle the subscription
        // (`register_exception_callback`) does the same — it refreshes and posts
        // out of band — and never sets this flag. This dirty path is the
        // fallback for a record with no handle / runtime, draining through the
        // single `monitor_status` owner.
        if self.status_dirty.swap(false, Ordering::AcqRel) {
            self.monitor_status();
        }

        // Every fresh (`pact == FALSE`) process clears ERRS before it decides
        // what to do: C `process()` calls `resetError` at the top of its idle
        // branch (asynRecord.c:339) and the callback it queues calls it again on
        // entry (:817) — both *above* the UCMD / ACMD / TMOD dispatch. So a
        // record that failed a transfer and is then scanned with TMOD=NoIO comes
        // back with an empty ERRS; the message does not outlive the cycle that
        // raised it. This port merges C's process and its callback, so one reset
        // here is both of C's.
        self.reset_error();

        // C dispatches on `state` FIRST, and the nesting is load-bearing:
        // `process()` is an if / else-if chain on `state` (asynRecord.c:331-361)
        // whose `stateIdle` arm holds everything else. `stateNoDevice` — the
        // record has no working port (`:312`, `:513`) — takes its own arm: it
        // reports "Not connect to a port" with STATE_ALARM/MINOR and queues
        // nothing, so a pending UCMD/ACMD stays pending for a cycle that has a
        // port to send it to.
        //
        // A record with no port therefore never even *looks* at the interrupt
        // flag; whatever a driver pushed before the port went away is discarded
        // by `done:` (the `process` wrapper above), not published. Placing the
        // interrupt gate above this refusal — which the wave-9 d9xg9 merge did,
        // on a misreading of C's :340-341 as textually-above-means-first — let a
        // portless record publish a stale interrupt value with NO_ALARM and an
        // empty ERRS, looking healthy and freshly updated.
        let Some(entry) = self.port_entry.clone() else {
            self.report_not_connected();
            return Ok(ProcessOutcome::complete());
        };

        // C `process()`, inside the `stateIdle` arm: "If we got value from
        // interrupt no need to read" (asynRecord.c:340-341) —
        // `if (pasynRecPvt->gotValue) goto done`. The cycle a driver interrupt
        // drove queues nothing: no UCMD/ACMD, no performIO, no queueRequest. It
        // stores the pushed value, posts its monitors (the framework's field
        // diffing, C's `monitor()`) and fires FLNK.
        //
        // The gate sits below `reset_error` (C's :339 precedes it) and above the
        // dispatch, because C's `goto done` jumps past the `queueRequest` that
        // would have run `asynCallbackProcess` at all.
        if let Some(sample) = self.io_intr.take_sample() {
            self.apply_io_intr_sample(sample);
            return Ok(ProcessOutcome::complete());
        }

        // C asynCallbackProcess (asynRecord.c:819-827) dispatches by priority: a
        // pending UCMD universal GPIB command first, else a pending ACMD addressed
        // GPIB command, else the TMOD/IFACE transfer (performIO). A GPIB command
        // therefore runs even when TMOD is NoIO, and it is not a transfer — the
        // IFACE gate below does not apply to it.
        let plan = if let Some(cycle) = self.take_gpib_cycle() {
            // gpibUniversalCmd / gpibAddressedCmd open with
            // `if (!pasynRec->gpibiv)` (asynRecord.c:1647-1651, :1693-1697): a port
            // with no asynGpib interface gets "No asynGpib interface" +
            // COMM_ALARM/MAJOR_ALARM and no bus traffic. The gate asks the port's
            // registry, like every other interface gate here; GPIBIV is the
            // operator's readback of the same answer.
            if !self.port_has(crate::interfaces::InterfaceType::Gpib) {
                self.report_no_interface(crate::interfaces::InterfaceType::Gpib.asyn_name());
                return Ok(ProcessOutcome::complete());
            }
            self.build_io_plan_for(Some(cycle))
        } else {
            if TransferMode::from_u16(self.tmod as u16) == TransferMode::NoIo {
                return Ok(ProcessOutcome::complete());
            }

            // `performIO` dispatches on IFACE and refuses the transfer when the
            // port does not implement the selected interface — "No asynInt32
            // interface" + COMM_ALARM/MAJOR_ALARM, no I/O (asynRecord.c:1328-1360).
            // A pure-octet transport (an IP socket, a serial line) has exactly one
            // valid interface, so this is the ordinary answer for a record pointed
            // at one with IFACE=Int32, not an edge case. The gate sits here, above
            // the blocking/non-blocking split, so both paths take the same refusal.
            let iface = InterfaceType::from_u16(self.iface as u16);
            if !self.has_interface(iface) {
                self.report_no_interface(iface.c_asyn_name());
                return Ok(ProcessOutcome::complete());
            }
            self.build_io_plan()
        };

        // C `process()` (asynRecord.c:342-353) queues `performIO`, then
        // `canBlock(&yesNo)`: a blocking port runs the I/O on the port
        // thread (`pact = TRUE; return`) and the record completes on the
        // callback re-process; a non-blocking port runs it inline (`goto
        // done`). Mirror that split — a `can_block` port with a live
        // database handle submits the I/O off the scan thread and re-enters
        // on completion; everything else (non-blocking port, or a record
        // not built into a database) keeps the synchronous inline path.
        let blocking_handle = entry.handle.can_block().then(|| entry.handle.clone());
        if let (Some(handle), Some((name, db))) = (blocking_handle, self.async_ctx.clone()) {
            return Ok(self.spawn_async_io(handle, name, db, plan));
        }

        self.perform_io(plan)?;
        Ok(ProcessOutcome::complete())
    }
}

impl Drop for AsynRecord {
    fn drop(&mut self) {
        // C removes `exceptCallback` when the record disconnects
        // (asynRecord.c:523,1154,1313); mirror that on teardown so a
        // dropped record leaves no dangling subscription in the
        // ExceptionManager callback list.
        self.clear_exception_callback();
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

    /// Every asynRecord `DBF_MENU` field must (a) serve enum choice strings,
    /// (b) DECLARE its native type as `Enum`, and (c) serve its value as an
    /// `Enum`. Measured against the compiled C IOC: a `DBF_MENU` field's native
    /// CA type is `DBF_ENUM` and `caget` returns the label, so declaring the
    /// field a `Short` makes `caget X.TMOD` answer `2` where C answers
    /// `Write/Read` and leaves every menu widget on the shipped asynRecord OPI
    /// screens unable to enumerate its states. The 34 names are the full
    /// `field(...,DBF_MENU)` set in asynRecord.dbd.
    #[test]
    fn menu_fields_are_declared_and_served_as_enum() {
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
            let desc = rec
                .field_list()
                .iter()
                .find(|d| d.name == *f)
                .unwrap_or_else(|| panic!("{f} must be declared"));
            assert_eq!(
                desc.dbf_type,
                DbFieldType::Enum,
                "{f} is DBF_MENU: its native type must be Enum, as in C"
            );
            assert!(
                matches!(rec.get_field(f), Some(EpicsValue::Enum(_))),
                "{f} must be served as an Enum so a client reads the label"
            );
        }
        // No DBF_MENU field may be left declared as a bare Short — the
        // declared-Enum set and the menu-choice set are the same 34 fields.
        for desc in rec.field_list() {
            if rec.menu_field_choices(desc.name).is_some() {
                assert_eq!(desc.dbf_type, DbFieldType::Enum, "{}", desc.name);
            } else {
                assert_ne!(desc.dbf_type, DbFieldType::Enum, "{}", desc.name);
            }
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

    /// UI32INP / UI32OUT / UI32MASK are C `DBF_ULONG` (asynRecord.dbd:335,
    /// :340, :346). Measured on the compiled C IOC, a DBF_ULONG field's native
    /// CA type is DBF_DOUBLE — CA has no unsigned-long wire type, so the IOC
    /// promotes it — which is exactly what `DbFieldType::ULong` serves. The
    /// boundary the previous signed `Long` declaration could not represent is
    /// the top bit: it read back as -1.
    #[test]
    fn ui32_fields_are_unsigned_long() {
        let mut rec = AsynRecord::default();
        for f in ["UI32INP", "UI32OUT", "UI32MASK"] {
            let desc = rec.field_list().iter().find(|d| d.name == f).unwrap();
            assert_eq!(desc.dbf_type, DbFieldType::ULong, "{f}");
        }
        // The default mask is all-ones: 4294967295, not -1.
        assert_eq!(
            rec.get_field("UI32MASK"),
            Some(EpicsValue::ULong(0xFFFF_FFFF))
        );
        // A top-bit-set value survives the round trip...
        rec.put_field("UI32OUT", EpicsValue::ULong(0x8000_0001))
            .unwrap();
        assert_eq!(
            rec.get_field("UI32OUT"),
            Some(EpicsValue::ULong(0x8000_0001))
        );
        // ...including from the DBR_DOUBLE form a CA client actually puts.
        rec.put_field("UI32MASK", EpicsValue::Double(4294967295.0))
            .unwrap();
        assert_eq!(
            rec.get_field("UI32MASK"),
            Some(EpicsValue::ULong(0xFFFF_FFFF))
        );
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
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 0);
        assert!(rec.errs.contains("not found"));
    }

    #[test]
    fn test_connect_empty_port() {
        let mut rec = AsynRecord::default();
        let _ = rec.connect_device();
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
        // C `process()` on stateNoDevice (asynRecord.c:357).
        assert_eq!(rec.errs, "Not connect to a port");
    }

    /// R10-46: the refusals C reports with `STATE_ALARM` must reach the record's
    /// severity, not only its ERRS. A record processed with no port takes
    /// `recGblSetSevr(pasynRec, STATE_ALARM, MINOR_ALARM)` on **every** such
    /// process (asynRecord.c:361), so it sits in STATE/MINOR rather than
    /// NO_ALARM while its port is down.
    #[test]
    fn a_process_with_no_port_raises_state_minor() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;

        rec.process().unwrap();
        assert_eq!(rec.errs, "Not connect to a port");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::STATE_ALARM, AlarmSeverity::Minor),
            "C asynRecord.c:361 alarms the stateNoDevice refusal STATE/MINOR"
        );

        // "on every such process": the second cycle alarms exactly like the first
        // (C re-runs recGblSetSevr each time; the record does not latch it away).
        rec.process().unwrap();
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::STATE_ALARM, AlarmSeverity::Minor),
            "the refusal alarm is re-raised on the next process, not one-shot"
        );
    }

    /// R10-46: the `AQR` cancel of a still-queued request is C's `wasQueued`
    /// branch (asynRecord.c:397-400) — "I/O request canceled" **and**
    /// `recGblSetSevr(pasynRec, STATE_ALARM, MAJOR_ALARM)`. The message and the
    /// severity are one event, so the outcome that carries the text to the
    /// completion re-entry carries the alarm with it.
    #[tokio::test]
    async fn a_canceled_queued_request_raises_state_major() {
        let entry = canblock_int32_entry(11);
        let cancel = CancelToken::new();
        // C `cancelRequest` with the request still queued: `wasQueued == true`.
        assert!(cancel.cancel(), "a queued request cancels");

        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        let plan = rec.build_io_plan();

        let out = run_io_plan(entry.handle.clone(), plan, cancel).await;
        rec.apply_io_outcome(out);

        assert_eq!(rec.errs, "I/O request canceled");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::STATE_ALARM, AlarmSeverity::Major),
            "C asynRecord.c:399 alarms the canceled request STATE/MAJOR"
        );
        assert_eq!(
            rec.i32inp, 0,
            "the canceled request performed no I/O — the device value never landed"
        );
    }

    /// R9-51: every ERRS text the record writes is one of C's, not an
    /// invented one. C's connect diagnostics are `connectDevice`'s own
    /// "Connect error, status=%d, %s" (asynRecord.c:1158) — where `%s` is the
    /// manager's `pasynUser->errorMessage` (asynManager.c:1331,1339) — and,
    /// on the `special()` PORT/ADDR/DRVINFO path only, the wrapper
    /// "connectDevice failed: %s" (asynRecord.c:515).
    #[test]
    fn connect_failure_errs_texts_are_c_texts() {
        // init / PCNCT path: connectDevice's own text.
        let mut rec = AsynRecord::default();
        rec.port = "NO_SUCH_PORT_R9_51".to_string();
        rec.init_record(1).unwrap();
        assert_eq!(
            rec.errs,
            "Connect error, status=3, asynManager:connectDevice port NO_SUCH_PORT_R9_51 not found"
        );

        // special() PORT path: the wrapper overwrites it.
        let mut rec = AsynRecord::default();
        rec.port = "NO_SUCH_PORT_R9_51".to_string();
        rec.special("PORT", true).unwrap();
        assert_eq!(
            rec.errs,
            "connectDevice failed: asynManager:connectDevice port NO_SUCH_PORT_R9_51 not found"
        );

        // An empty PORT reaches the manager's other rejection.
        let mut rec = AsynRecord::default();
        rec.special("PORT", true).unwrap();
        assert_eq!(
            rec.errs,
            "connectDevice failed: asynManager:connectDevice no port name provided"
        );
    }

    /// R10-47: the list this port has to state — the dbd is C's source of truth
    /// for which puts reach `special()`, and this port's database calls `special`
    /// for every put — must be the dbd's, exactly.
    #[test]
    fn spc_mod_fields_match_the_dbd() {
        // asynRecord.dbd: every `field(...) { ... special(SPC_MOD) ... }`.
        let dbd = [
            "PORT", "ADDR", "PCNCT", "DRVINFO", "REASON", "IFACE", "OEOS", "IEOS", "UI32MASK",
            "BAUD", "LBAUD", "PRTY", "DBIT", "SBIT", "MCTL", "FCTL", "IXON", "IXOFF", "IXANY",
            "HOSTINFO", "DRTO", "TMSK", "TB0", "TB1", "TB2", "TB3", "TB4", "TB5", "TIOM", "TIB0",
            "TIB1", "TIB2", "TINM", "TINB0", "TINB1", "TINB2", "TINB3", "TSIZ", "TFIL", "AUCT",
            "CNCT", "ENBL", "AQR",
        ];
        let mut ours = SPC_MOD_FIELDS.to_vec();
        let mut theirs = dbd.to_vec();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
    }

    /// R10-47: C `process()` resets ERRS at the top of its idle branch
    /// (asynRecord.c:339) and again in the callback it queues (:817) — both
    /// *above* the TMOD dispatch. So a record that failed a transfer and is then
    /// scanned with TMOD=NoIO comes back with an empty ERRS: the message does not
    /// outlive the cycle that raised it.
    ///
    /// The record needs a port for that: C's idle branch — the one that resets
    /// and then dispatches nothing for TMOD=NoIO — is reached only in
    /// `stateIdle`. A record with no port is `stateNoDevice` and reports "Not
    /// connect to a port" on every scan, whatever TMOD says (:356-357).
    #[test]
    fn a_noio_process_clears_a_stale_errs() {
        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let (mut rec, _rt) = gpib_record(
            "r10_47_noio",
            GpibSpyPort::new("r10_47_noio", calls.clone()),
        );
        rec.tmod = TransferMode::NoIo as i32;
        rec.errs = "Read error, timeout".to_string();

        rec.process().unwrap();

        assert_eq!(
            rec.errs, "",
            "C process() resetError (asynRecord.c:339) runs above the TMOD check"
        );
    }

    /// R10-47: C `special()` opens with `resetError` (asynRecord.c:390), before
    /// any field dispatch — so an SPC_MOD put starts with a clean ERRS and only
    /// its own diagnostic (here: none) is in it when it returns.
    #[test]
    fn a_special_put_clears_a_stale_errs() {
        let mut rec = AsynRecord::default();
        rec.errs = "Write error, nout=0, timeout".to_string();
        rec.tmsk = 0;

        rec.special("TMSK", true).unwrap();

        assert_eq!(
            rec.errs, "",
            "C special() resetError (asynRecord.c:390) runs before the field dispatch"
        );
    }

    /// R10-47 boundary: a put to a field the dbd does NOT mark `special(SPC_MOD)`
    /// never reaches C `special()` at all, so it must not reset ERRS — including
    /// a put to ERRS itself, which would otherwise erase what the operator just
    /// wrote. This is what [`SPC_MOD_FIELDS`] gates.
    #[test]
    fn a_non_spc_mod_put_keeps_errs() {
        let mut rec = AsynRecord::default();
        rec.errs = "Read error, timeout".to_string();

        rec.special("VAL", true).unwrap();
        rec.special("ERRS", true).unwrap();

        assert_eq!(
            rec.errs, "Read error, timeout",
            "a non-SPC_MOD put does not reach C special() and does not reset ERRS"
        );
    }

    /// R10-47: `connectDevice` clears at its ENTRY (asynRecord.c:1151), so the
    /// previous attempt's diagnostic never survives a new one. The port's old
    /// end-of-function clear was conditional on `resolved_reason != 0`, which
    /// left a stale message in place for the driver whose DRVINFO resolves to
    /// parameter 0 — a legitimate reason.
    #[test]
    fn connect_device_entry_clears_the_previous_attempts_errs() {
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct ParamDriver(PortDriverBase);
        impl PortDriver for ParamDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "r10_47_reason0";
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        // The port's first parameter — reason 0.
        assert_eq!(
            base.params.create_param("FIRST", ParamType::Int32).unwrap(),
            0
        );
        let (tx, rx) = mpsc::channel(16);
        let actor = PortActor::new(Box::new(ParamDriver(base)), rx);
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "FIRST".to_string();
        rec.errs = "Connect error, status=3, port down".to_string();

        rec.connect_device().unwrap();

        assert_eq!(rec.resolved_reason, 0, "DRVINFO resolved to parameter 0");
        assert_eq!(
            rec.errs, "",
            "a successful connect leaves no diagnostic behind, whatever the reason resolves to"
        );
    }

    /// R12-47: the record's HOSTINFO put and its connect-time option readback are
    /// the two requests C queues at `asynQueuePriorityConnect` carrying
    /// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (asynRecord.c:566-569,
    /// :1277-1280), so both run on a port that is *down* — the HOSTINFO put is
    /// the operator's only route to repoint an IP port aimed at a wrong or moved
    /// host, and such a port is disconnected by definition.
    ///
    /// C's asymmetry is the other half of the test: an ordinary option put (BAUD)
    /// and the EOS put are queued at Low priority with no waiver, so they still
    /// take `asynDisconnected` from the queue gate.
    #[test]
    fn hostinfo_put_and_option_readback_run_on_a_disconnected_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        #[derive(Default)]
        struct Log {
            option_sets: Vec<(String, String)>,
            eos_sets: Vec<Vec<u8>>,
        }

        struct DeadLineDriver {
            base: PortDriverBase,
            log: Arc<Mutex<Log>>,
            host: Arc<Mutex<String>>,
        }
        impl PortDriver for DeadLineDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                key: &str,
                value: &str,
            ) -> crate::error::AsynResult<()> {
                self.log
                    .lock()
                    .unwrap()
                    .option_sets
                    .push((key.to_string(), value.to_string()));
                if key == "hostinfo" {
                    *self.host.lock().unwrap() = value.to_string();
                }
                Ok(())
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                match key {
                    // A serial/IP driver answers its configured line settings from
                    // its own state; none of it needs the wire to be up.
                    "baud" => Ok("9600".to_string()),
                    "parity" => Ok("even".to_string()),
                    "hostinfo" => Ok(self.host.lock().unwrap().clone()),
                    _ => Err(crate::error::AsynError::Status {
                        status: crate::error::AsynStatus::Error,
                        message: format!("unsupported option {key}"),
                    }),
                }
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                self.log.lock().unwrap().eos_sets.push(eos.to_vec());
                Ok(())
            }
        }

        let port_name = "r12_47_dead_line";
        let log = Arc::new(Mutex::new(Log::default()));
        let host = Arc::new(Mutex::new("oldhost:5000".to_string()));
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        // The port is down and nothing will bring it back: C's `autoConnectDevice`
        // is not consulted for a waived request, and must not be needed for one.
        base.auto_connect = false;
        base.set_connected(false);
        let (tx, rx) = mpsc::channel(64);
        let actor = PortActor::new(
            Box::new(DeadLineDriver {
                base,
                log: log.clone(),
                host: host.clone(),
            }),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();

        // The connect-time option readback ran on the down port (C :1277-1280).
        assert_eq!(
            rec.baud,
            baud_choice_index("9600"),
            "C queues getOptions with QUEUE_EVEN_IF_NOT_CONNECTED, so BAUD is the \
             port's real rate, not Unknown"
        );
        assert_eq!(rec.prty, 2, "…and PRTY likewise");
        assert_eq!(rec.hostinfo, "oldhost:5000", "…and HOSTINFO likewise");

        // The HOSTINFO put reaches the driver, and the fall-through readback comes
        // back off the same down port.
        log.lock().unwrap().option_sets.clear();
        rec.hostinfo = "newhost:5000".to_string();
        rec.special("HOSTINFO", true).unwrap();
        assert_eq!(
            log.lock().unwrap().option_sets,
            vec![("hostinfo".to_string(), "newhost:5000".to_string())],
            "the only route to repoint a dead IP port must not be refused"
        );
        assert_eq!(rec.errs, "", "a waived put reports no error");
        assert_eq!(rec.hostinfo, "newhost:5000");

        // An ordinary option put carries no waiver: C queues it at Low priority
        // (asynRecord.c:560-564) and the gate refuses it.
        log.lock().unwrap().option_sets.clear();
        rec.baud = baud_choice_index("19200");
        rec.special("BAUD", true).unwrap();
        assert!(
            log.lock().unwrap().option_sets.is_empty(),
            "a BAUD put on a disconnected port is asynDisconnected in C, not a driver call"
        );
        // The refusal is `queueRequest`'s return value, so C reports
        // `pasynUserSpecial->errorMessage` (asynRecord.c:571-576) — *not* the
        // "Error setting option, %s" text of a `setOption` that ran (R14-46).
        assert!(
            rec.errs.contains("not connected"),
            "the gate's refusal message is what reaches ERRS: {}",
            rec.errs
        );
        assert!(
            !rec.errs.starts_with("Error setting option"),
            "a refused put never entered setOption: {}",
            rec.errs
        );

        // Nor does the EOS put — C keeps it at Low priority with no waiver
        // (asynRecord.c:1296 for the readback half), so IEOS/OEOS stay refused on
        // a down port. Preserving that asymmetry is part of the parity.
        rec.ieos = "\\r\\n".to_string();
        rec.special("IEOS", true).unwrap();
        assert!(
            log.lock().unwrap().eos_sets.is_empty(),
            "the EOS put must not inherit the HOSTINFO waiver"
        );
    }

    /// R10-48: an option put is dispatched whatever its value — C `setOption`
    /// has a `case` per field and no value gate (asynRecord.c:1777-1826), so the
    /// index-0 "Unknown" of each menu goes to the driver like any other choice
    /// (`baud_choices[0]` is the literal "Unknown", :49), the driver's rejection
    /// is reported, and the `/* no break */` fall-through into `getOptions`
    /// (:845-849) snaps every option readback back to the port's real values.
    ///
    /// The port used to `return` on those values: the driver never saw the write
    /// and the record kept showing a setting the port had never taken.
    #[test]
    fn an_unknown_option_put_reaches_the_driver_and_refreshes_the_readbacks() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        /// Logs every `setOption` it is given and refuses the ones it cannot
        /// parse — a real serial driver's answer to "Unknown"
        /// (drvAsynSerialPort.c:355-372 reports "Bad baud rate").
        struct LoggingDriver {
            base: PortDriverBase,
            sets: Arc<Mutex<Vec<(String, String)>>>,
        }
        impl PortDriver for LoggingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                key: &str,
                value: &str,
            ) -> crate::error::AsynResult<()> {
                self.sets
                    .lock()
                    .unwrap()
                    .push((key.to_string(), value.to_string()));
                if value == "Unknown" {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("Bad {key}"),
                    });
                }
                Ok(())
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                match key {
                    "baud" => Ok("9600".to_string()),
                    "parity" => Ok("even".to_string()),
                    _ => Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unsupported option {key}"),
                    }),
                }
            }
        }

        let port_name = "r10_48_unknown_option";
        let sets = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(64);
        let actor = PortActor::new(
            Box::new(LoggingDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                sets: sets.clone(),
            }),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        sets.lock().unwrap().clear();

        // The operator puts BAUD = Unknown (menu index 0) on a port running at
        // 9600, with PRTY already read back as "even".
        assert_eq!(rec.baud, baud_choice_index("9600"));
        assert_eq!(rec.prty, 2);
        rec.baud = 0;
        rec.special("BAUD", true).unwrap();

        assert_eq!(
            *sets.lock().unwrap(),
            vec![("baud".to_string(), "Unknown".to_string())],
            "C sends baud_choices[0] = \"Unknown\" to the driver (asynRecord.c:1780)"
        );
        assert_eq!(
            rec.errs, "Error setting option, Bad baud",
            "the driver's rejection is C's \"Error setting option, %s\" (asynRecord.c:1828-1830)"
        );
        assert_eq!(
            rec.baud,
            baud_choice_index("9600"),
            "the getOptions fall-through snaps BAUD back to the port's real rate"
        );
        assert_eq!(rec.lbaud, 9600, "…and LBAUD with it");
        assert_eq!(
            rec.prty, 2,
            "every option readback refreshes, not just BAUD"
        );

        // Same for a string option C sends unconditionally: an empty HOSTINFO is
        // a real setOption("hostInfo", "") (asynRecord.c:1824-1826).
        sets.lock().unwrap().clear();
        rec.hostinfo = String::new();
        rec.special("HOSTINFO", true).unwrap();
        assert_eq!(
            *sets.lock().unwrap(),
            vec![("hostinfo".to_string(), String::new())],
            "C gates the HOSTINFO set on nothing — the driver decides"
        );
    }

    /// R9-51: TFIL open failure reports the path alone — C has no OS message
    /// to splice (asynRecord.c:465-466).
    #[test]
    fn trace_file_open_failure_errs_text_is_the_c_text() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct TfilDriver(PortDriverBase);
        impl PortDriver for TfilDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "r9_51_tfil";
        let (tx, rx) = mpsc::channel(16);
        let actor = PortActor::new(
            Box::new(TfilDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.tfil = "/nonexistent-dir-r9-51/trace.log".to_string();
        rec.special("TFIL", true).unwrap();
        assert_eq!(
            rec.errs,
            "Error opening trace file: /nonexistent-dir-r9-51/trace.log"
        );
    }

    use crate::port::{PortDriver, PortDriverBase, PortFlags};

    /// R10-55. The bus traffic a GPIB spy port saw — C's asynGpibPort, reduced
    /// to the three methods asynRecord's UCMD / ACMD paths can reach.
    #[derive(Default)]
    struct GpibCalls {
        universal: Vec<u8>,
        addressed: Vec<Vec<u8>>,
        reads: usize,
    }

    /// A port that registers asynGpib (as `pasynGpib->registerPort` does,
    /// asynGpib.c:562-631) and records what the record sends it.
    struct GpibSpyPort {
        base: PortDriverBase,
        calls: Arc<Mutex<GpibCalls>>,
        /// The serial-poll response byte the device returns, if any.
        poll_byte: Option<u8>,
        /// When set, every GPIB command fails with this driver text — C's
        /// driver-error branch (`pasynUser->errorMessage` in the ERRS tail).
        fail: Option<String>,
    }

    impl GpibSpyPort {
        fn new(name: &str, calls: Arc<Mutex<GpibCalls>>) -> Self {
            Self {
                base: PortDriverBase::new(name, 1, PortFlags::default()),
                calls,
                poll_byte: None,
                fail: None,
            }
        }

        fn check(&self) -> AsynResult<()> {
            match &self.fail {
                Some(msg) => Err(crate::error::AsynError::Status {
                    status: crate::error::AsynStatus::Error,
                    message: msg.clone(),
                }),
                None => Ok(()),
            }
        }
    }

    impl PortDriver for GpibSpyPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
            crate::interfaces::gpib::gpib_port_capabilities()
        }
        fn gpib_universal_cmd(&mut self, _user: &mut AsynUser, cmd: u8) -> AsynResult<()> {
            self.calls.lock().unwrap().universal.push(cmd);
            self.check()
        }
        fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
            self.calls.lock().unwrap().addressed.push(data.to_vec());
            self.check()
        }
        fn read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
            self.calls.lock().unwrap().reads += 1;
            match self.poll_byte {
                Some(b) if !buf.is_empty() => {
                    buf[0] = b;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    /// Register a GPIB spy port and attach a record to it. The runtime handle
    /// comes back with the record: dropping it shuts the port actor down, and
    /// the record's next request would fail on a dead reply channel.
    fn gpib_record(
        port_name: &str,
        port: GpibSpyPort,
    ) -> (AsynRecord, crate::runtime::PortRuntimeHandle) {
        use crate::runtime::{RuntimeConfig, create_port_runtime};
        let (rt, _jh) = create_port_runtime(port, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        (rec, rt)
    }

    /// R10-55. C asynCallbackProcess (asynRecord.c:819-822) dispatches a pending
    /// UCMD to `gpibUniversalCmd`, which sends the menu's command byte through
    /// `pasynGpib->universalCmd` (:1672) and then resets UCMD to None. The
    /// command takes priority over performIO, so it runs even under TMOD=NoIO.
    #[test]
    fn process_ucmd_sends_the_universal_command_to_the_port() {
        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let (mut rec, _rt) = gpib_record(
            "r10_55_ucmd",
            GpibSpyPort::new("r10_55_ucmd", calls.clone()),
        );

        rec.tmod = TransferMode::NoIo as i32;
        rec.ucmd = 1; // Device Clear (DCL)
        rec.process().unwrap();

        assert_eq!(
            calls.lock().unwrap().universal,
            vec![crate::interfaces::gpib::IBDCL]
        );
        assert_eq!(rec.errs, "");
        assert_eq!(rec.ucmd, 0, "UCMD resets to None after dispatch");
    }

    /// R10-55. A pending ACMD reaches `pasynGpib->addressedCmd` with the frame C
    /// builds in `acmd[]` — `[UNT, UNL, addr + LADBASE, cmd, UNT, UNL]`
    /// (asynRecord.c:1698-1716, :1750).
    #[test]
    fn process_acmd_sends_the_addressed_frame_to_the_port() {
        use crate::interfaces::gpib::{IBGET, IBUNL, IBUNT, LADBASE};

        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let (mut rec, _rt) = gpib_record(
            "r10_55_acmd",
            GpibSpyPort::new("r10_55_acmd", calls.clone()),
        );

        rec.addr = 7;
        rec.acmd = 1; // Group Execute Trigger (GET)
        rec.process().unwrap();

        assert_eq!(
            calls.lock().unwrap().addressed,
            vec![vec![IBUNT, IBUNL, 7 + LADBASE, IBGET, IBUNT, IBUNL]]
        );
        assert_eq!(rec.errs, "");
        assert_eq!(rec.acmd, 0, "ACMD resets to None after dispatch");
    }

    /// R10-55. ACMD = Serial Poll is three operations, not one frame: universal
    /// SPE, a one-byte octet read into SPR, universal SPD
    /// (asynRecord.c:1717-1746).
    #[test]
    fn process_acmd_serial_poll_runs_spe_read_spd() {
        use crate::interfaces::gpib::{IBSPD, IBSPE};

        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let mut port = GpibSpyPort::new("r10_55_poll", calls.clone());
        port.poll_byte = Some(0x41);
        let (mut rec, _rt) = gpib_record("r10_55_poll", port);

        rec.acmd = 5; // Serial Poll
        rec.process().unwrap();

        let seen = calls.lock().unwrap();
        assert_eq!(seen.universal, vec![IBSPE, IBSPD]);
        assert_eq!(seen.reads, 1, "one status-byte read between SPE and SPD");
        assert!(seen.addressed.is_empty(), "serial poll sends no ACMD frame");
        drop(seen);
        assert_eq!(rec.spr, 0x41, "the status byte lands in SPR");
        assert_eq!(rec.errs, "");
    }

    /// R10-55. A driver that refuses the command puts its own text in ERRS
    /// behind C's format — "GPIB Universal command %s" — and raises
    /// WRITE_ALARM/MAJOR (asynRecord.c:1673-1677).
    #[test]
    fn a_failing_universal_command_reports_the_driver_text() {
        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let mut port = GpibSpyPort::new("r10_55_ucmd_fail", calls.clone());
        port.fail = Some("prologixUniversalCmd unimplemented".to_string());
        let (mut rec, _rt) = gpib_record("r10_55_ucmd_fail", port);

        rec.ucmd = 1;
        rec.process().unwrap();

        assert_eq!(
            rec.errs,
            "GPIB Universal command prologixUniversalCmd unimplemented"
        );
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::WRITE_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);
    }

    /// R10-55. The negative control: a port that never registered asynGpib —
    /// every octet transport (IP socket, serial line) — refuses the command with
    /// "No asynGpib interface" + COMM_ALARM/MAJOR and sends nothing, C's
    /// `if (!pasynRec->gpibiv)` branch (asynRecord.c:1647-1651, :1693-1697). The
    /// menu field is still consumed: C resets it in the caller either way
    /// (:822, :826).
    #[test]
    fn a_port_without_asyngpib_refuses_ucmd_and_acmd() {
        use crate::interfaces::octet_transport_capabilities;
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct OctetTransport(PortDriverBase);
        impl PortDriver for OctetTransport {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                octet_transport_capabilities()
            }
        }

        let port_name = "r10_55_no_gpib";
        let (rt, _jh) = create_port_runtime(
            OctetTransport(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.gpibiv, 0, "an octet transport has no asynGpib");

        rec.tmod = TransferMode::NoIo as i32;
        rec.ucmd = 1;
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynGpib interface");
        assert_eq!(rec.ucmd, 0, "UCMD is consumed even when refused");
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);

        rec.acmd = 1;
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynGpib interface");
        assert_eq!(rec.acmd, 0, "ACMD is consumed even when refused");
    }

    /// R10-55. C tests `state == stateNoDevice` (asynRecord.c:356-357) *above*
    /// the queued callback that dispatches UCMD/ACMD, so a record with no port
    /// dispatches nothing and its pending command survives for a cycle that has
    /// a port.
    #[test]
    fn a_record_with_no_port_leaves_ucmd_pending() {
        let mut rec = AsynRecord::default();
        rec.ucmd = 1;
        rec.process().unwrap();

        assert_eq!(rec.errs, "Not connect to a port");
        assert_eq!(
            rec.ucmd, 1,
            "nothing was dispatched, so nothing is consumed"
        );
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::STATE_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Minor);
    }

    #[test]
    fn process_ucmd_takes_priority_over_acmd() {
        // C dispatches UCMD first (`if`), ACMD only in the `else if`. With
        // both pending only UCMD is consumed this cycle; ACMD is left for
        // the next process.
        let calls = Arc::new(Mutex::new(GpibCalls::default()));
        let (mut rec, _rt) = gpib_record(
            "r10_55_priority",
            GpibSpyPort::new("r10_55_priority", calls.clone()),
        );

        rec.ucmd = 1;
        rec.acmd = 1;
        rec.process().unwrap();
        assert_eq!(rec.ucmd, 0, "UCMD consumed first");
        assert_eq!(rec.acmd, 1, "ACMD left pending while UCMD was set");
        assert_eq!(
            calls.lock().unwrap().universal,
            vec![crate::interfaces::gpib::IBDCL],
            "only the universal command went to the bus"
        );
        assert!(calls.lock().unwrap().addressed.is_empty());
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

    /// C `asynRecord.c` init_record pass 0 seeds `tfil = "Unknown"`
    /// unconditionally — no port, no connect. The `.dbd` carries no
    /// `initial(...)` for TFIL, so a faithful port must serve the constant from
    /// record code. This is the record Path 1 serves (the oracle's asyn
    /// reproducer), where the base display stub's seed does not reach.
    #[test]
    fn tfil_serves_unknown_with_no_live_port() {
        let rec = AsynRecord::default();
        // No port set, no connect_device, no monitor_status — the raw
        // construction value a client reads.
        assert_eq!(rec.tfil, "Unknown");
        assert_eq!(
            rec.get_field("TFIL"),
            Some(EpicsValue::String("Unknown".into())),
            "get(TFIL) must serve C's init_record seed with no live port"
        );
    }

    /// End-to-end: a freshly loaded asyn record — no port, never processed —
    /// reads UDF=0 / STAT=NO_ALARM / SEVR=NO_ALARM, C `asynRecord.c`
    /// init_record pass 0's `pasynRec->udf = 0; recGblResetAlarms(pasynRec)`.
    /// Built through `IocBuilder` so the init owner's `run_init_passes` (where
    /// [`Record::init_resets_alarms`] is honoured) actually runs — the record
    /// struct alone cannot reach the common UDF/STAT/SEVR fields.
    #[tokio::test]
    async fn init_resets_alarms_to_defined_no_alarm() {
        use epics_base_rs::server::ioc_builder::IocBuilder;
        use std::collections::HashMap;

        let macros = HashMap::new();
        let (name, factory) = crate::asyn_record::asyn_record_factory();
        let (db, _autosave) = IocBuilder::new()
            .register_record_type(name, factory)
            .db_string("record(asyn, \"TEST:ASYN:UDF\") {}\n", &macros)
            .unwrap()
            .build()
            .await
            .unwrap();

        let rec = db.get_record("TEST:ASYN:UDF").expect("asyn record loaded");
        let inst = rec.read();
        assert_eq!(
            inst.get_common_field("UDF"),
            Some(EpicsValue::UChar(0)),
            "C init_record pass 0: udf=0 (a device-config record is defined at load)"
        );
        assert_eq!(
            inst.get_common_field("STAT"),
            Some(EpicsValue::Short(0)),
            "recGblResetAlarms → STAT=NO_ALARM"
        );
        assert_eq!(
            inst.get_common_field("SEVR"),
            Some(EpicsValue::Short(0)),
            "recGblResetAlarms → SEVR=NO_ALARM"
        );
    }

    /// R9-52: C `monitorStatus` refreshes TSIZ from `getTraceIOTruncateSize`
    /// (asynRecord.c:1100) and writes TFIL = "Unknown" when the port's trace
    /// sink is no longer the one this record installed (:1119-1124). Neither
    /// readback was refreshed on any path.
    #[test]
    fn monitor_status_refreshes_tsiz_and_flags_a_foreign_trace_file() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct TsizDriver(PortDriverBase);
        impl PortDriver for TsizDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "r9_52_tsiz";
        let (tx, rx) = mpsc::channel(16);
        let actor = PortActor::new(
            Box::new(TsizDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        let trace = Arc::new(TraceManager::new());
        register_port(port_name, handle, trace.clone()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        // The default truncate size lands in TSIZ on connect, and the first
        // sample seeds the sink identity: no spurious foreign flip. TFIL holds
        // its init-record seed ("Unknown") — the trace file has not changed.
        assert_eq!(rec.tsiz, 80);
        assert_eq!(rec.tfil, "Unknown");

        // A foreign `asynSetTraceIOTruncateSize` — the iocsh path, not this
        // record — must show up in TSIZ on the next monitorStatus.
        trace.set_io_truncate_size(Some(port_name), 17);
        rec.monitor_status();
        assert_eq!(rec.tsiz, 17);
        assert_eq!(rec.tfil, "Unknown", "the trace file did not change");

        // The record's own TFIL write must NOT read back as foreign.
        rec.tfil = "<stdout>".to_string();
        rec.special("TFIL", true).unwrap();
        rec.monitor_status();
        assert_eq!(rec.tfil, "<stdout>");

        // A foreign `asynSetTraceFile` re-points the sink: C cannot know the
        // new name, so TFIL becomes "Unknown".
        trace.set_trace_file(Some(port_name), TraceFile::Stderr);
        rec.monitor_status();
        assert_eq!(rec.tfil, "Unknown");
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, "test_asyn_rec".into(), interrupts, actor_id);
        let trace = Arc::new(TraceManager::new());

        register_port("test_asyn_rec", handle, trace).unwrap();

        let entry = crate::registry::get_port("test_asyn_rec");
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let mut handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        handle.set_capabilities(true, 4);
        let trace = Arc::new(TraceManager::new());
        register_port(port_name, handle, trace.clone()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.addr = 3;
        let _ = rec.connect_device();
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
        let _ = rec0.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        let trace = Arc::new(TraceManager::new());
        // Non-default info mask set on the manager BEFORE the record connects.
        trace.set_trace_info_mask(
            Some(port_name),
            TraceInfoMask::SOURCE | TraceInfoMask::THREAD,
        );
        register_port(port_name, handle, trace).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        let trace = Arc::new(TraceManager::new());
        // Wire the exception sink as a real IOC would (PortManager installs
        // it); without it the record's subscription is a no-op.
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::TIME);
        register_port(port_name, handle, trace.clone()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32; // process() does no I/O
        let _ = rec.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        // ASCII read: AINP gets the (lossy) text; BINP must stay untouched.
        let mut ascii = AsynRecord::default();
        ascii.port = port_name.to_string();
        let _ = ascii.connect_device();
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
        let _ = binary.connect_device();
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

    /// R17-48. Every escaped record field is `epicsStrSnPrintEscaped` into a
    /// *sized* C buffer, and the size is the field's own: TINP is
    /// `sizeof(pasynRec->tinp)` = 40 (asynRecord.c:725,:1629), IEOS/OEOS are
    /// `EOS_SIZE` = 10 (`:68`, `:1990-1991`). What does not fit is cut at
    /// `dstlen - 1` *characters* — inside an escape pair if that is where the
    /// count lands, which is why C's own TINP can end in a lone backslash.
    /// Verified against compiled libCom (`epicsStrnEscapedFromRaw`: 200 raw CRLF
    /// bytes, `dstlen = 40` → 39 chars ending `\r\n\r\`).
    #[test]
    fn escaped_record_fields_are_cut_at_their_c_buffer_size() {
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct LongDriver(PortDriverBase);
        impl PortDriver for LongDriver {
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
                // 100 CRLF pairs: 400 escaped characters into a 40-byte TINP.
                let payload: Vec<u8> = b"\r\n".repeat(100);
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                Ok((n, EomReason::END))
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Ok(())
            }
            fn get_input_eos(&self, _user: &AsynUser) -> Vec<u8> {
                // 12 escaped characters into a 10-byte inputEosTranslate.
                b"\r\n\r\n\r\n".to_vec()
            }
        }

        let port_name = "r17_48_escape_bounds";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(LongDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.ifmt = ASYN_FMT_BINARY;
        rec.imax = 256;
        rec.process().unwrap();
        assert_eq!(rec.errs, "");
        assert_eq!(rec.nord, 200, "NORD is the raw transfer count, unbounded");
        assert_eq!(
            rec.tinp.len(),
            TINP_SIZE - 1,
            "TINP is a 40-byte DBF_STRING"
        );
        assert!(
            rec.tinp.ends_with(r"\r\n\r\"),
            "C cuts mid escape pair and leaves the backslash: {}",
            rec.tinp
        );

        // The EOS readback runs through the same escape with the tighter
        // EOS_SIZE bound: six raw bytes escape to twelve characters, nine fit.
        rec.ieos = r"\r".to_string();
        rec.special("IEOS", true).unwrap();
        assert_eq!(rec.errs, "");
        assert_eq!(rec.ieos, r"\r\n\r\n\", "IEOS is cut at EOS_SIZE - 1");
        assert_eq!(rec.ieos.len(), EOS_SIZE - 1);
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        // ASCII read failure: NORD/EOMR cleared to 0, AINP/TINP cleared to "",
        // and BINP (not the IFMT-selected field) left as it was — matching C,
        // which memsets only `inptr` (the ASCII buffer here).
        let mut ascii = AsynRecord::default();
        ascii.port = port_name.to_string();
        let _ = ascii.connect_device();
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
        let _ = binary.connect_device();
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
        let _ = writer.connect_device();
        writer.iface = 0;
        writer.tmod = TransferMode::Write as i32;
        writer.ofmt = ASYN_FMT_ASCII;
        writer.aout = "hello".to_string();
        writer.nawt = 9;
        writer.process().unwrap();
        assert!(!writer.errs.is_empty(), "write error must set ERRS");
        assert_eq!(writer.nawt, 0, "failed write must reset NAWT to 0");
    }

    /// R9-48: each C register handler reports its own diagnostic —
    /// "Int32 write error, %s" / "Int32 read error, %s" (asynRecord.c:1378,
    /// :1391) and the UInt32 / Float64 twins (:1414/:1429, :1450/:1463) — with
    /// `pasynUser->errorMessage` as the tail. The port emitted a generic
    /// "write: {e}" / "read: {e}", which told the operator neither which
    /// interface failed nor what the driver said (`{e}` Display prefixes the
    /// Rust status debug onto the driver text).
    ///
    /// The same C `reportError` anchor covers the option and EOS puts:
    /// `setOption` reports "Error setting option, %s" (:1826) and `setEos`
    /// reports "Error setting input eos, %s" / "Error setting output eos, %s"
    /// (:1971, :1979); the port invented "set_option(baud): ..." /
    /// "set_input_eos: ...".
    #[test]
    fn register_and_option_errors_use_the_c_errs_text() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        fn boom(what: &str) -> AsynError {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{what} boom"),
            }
        }

        /// Every register read/write, the option set and both EOS sets fail
        /// with a driver message.
        struct AllFailDriver(PortDriverBase);
        impl PortDriver for AllFailDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn write_int32(&mut self, _u: &mut AsynUser, _v: i32) -> crate::error::AsynResult<()> {
                Err(boom("i32w"))
            }
            fn read_int32(&mut self, _u: &AsynUser) -> crate::error::AsynResult<i32> {
                Err(boom("i32r"))
            }
            fn write_uint32_digital(
                &mut self,
                _u: &mut AsynUser,
                _v: u32,
                _m: u32,
            ) -> crate::error::AsynResult<()> {
                Err(boom("u32w"))
            }
            fn read_uint32_digital(
                &mut self,
                _u: &AsynUser,
                _m: u32,
            ) -> crate::error::AsynResult<u32> {
                Err(boom("u32r"))
            }
            fn write_float64(
                &mut self,
                _u: &mut AsynUser,
                _v: f64,
            ) -> crate::error::AsynResult<()> {
                Err(boom("f64w"))
            }
            fn read_float64(&mut self, _u: &AsynUser) -> crate::error::AsynResult<f64> {
                Err(boom("f64r"))
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                _k: &str,
                _v: &str,
            ) -> crate::error::AsynResult<()> {
                Err(boom("opt"))
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Err(boom("ieos"))
            }
            fn set_output_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Err(boom("oeos"))
            }
        }

        let port_name = "test_register_errs_text";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(AllFailDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let reg = |iface: InterfaceType, tmod: TransferMode| -> String {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            let _ = rec.connect_device();
            rec.iface = iface as i32;
            rec.tmod = tmod as i32;
            rec.process().unwrap();
            rec.errs.clone()
        };

        assert_eq!(
            reg(InterfaceType::Int32, TransferMode::Write),
            "Int32 write error, i32w boom"
        );
        assert_eq!(
            reg(InterfaceType::Int32, TransferMode::Read),
            "Int32 read error, i32r boom"
        );
        assert_eq!(
            reg(InterfaceType::UInt32Digital, TransferMode::Write),
            "UInt32 write error, u32w boom",
            "C writes UInt32, not UInt32Digital"
        );
        assert_eq!(
            reg(InterfaceType::UInt32Digital, TransferMode::Read),
            "UInt32 read error, u32r boom"
        );
        assert_eq!(
            reg(InterfaceType::Float64, TransferMode::Write),
            "Float64 write error, f64w boom"
        );
        assert_eq!(
            reg(InterfaceType::Float64, TransferMode::Read),
            "Float64 read error, f64r boom"
        );

        // Option / EOS puts — the same C reportError anchor.
        let mut opt = AsynRecord::default();
        opt.port = port_name.to_string();
        let _ = opt.connect_device();
        opt.lbaud = 9600;
        opt.special("LBAUD", true).unwrap();
        assert_eq!(opt.errs, "Error setting option, opt boom");

        opt.ieos = "\\n".to_string();
        opt.special("IEOS", true).unwrap();
        assert_eq!(opt.errs, "Error setting input eos, ieos boom");

        opt.oeos = "\\r".to_string();
        opt.special("OEOS", true).unwrap();
        assert_eq!(opt.errs, "Error setting output eos, oeos boom");
        // The failed EOS sets fall through to the C `getEos` re-read (R9-47),
        // which finds no EOS in this driver and empties both fields.
        assert_eq!(opt.ieos, "");
        assert_eq!(opt.oeos, "");
    }

    /// R9-47: C `asynCallbackSpecial` falls *through* `callbackSetOption` into
    /// `callbackGetOption` (asynRecord.c:845-849) and `callbackSetEos` into
    /// `callbackGetEos` (:851-855) — the `/* no break */` comments. So every
    /// option / EOS put ends with `getOptions` (:1834) / `getEos` (:1985)
    /// re-reading the driver's *actual* values into the record fields and
    /// POST_IF_NEWing them, even when the set failed. The port set and never
    /// re-read, so the record advertised the operator's request: a driver that
    /// only runs at 9600 still showed LBAUD=115200, and a rejected EOS stayed
    /// in IEOS as though the driver had taken it.
    ///
    /// `getEos` re-reads *both* EOS strings (:2003, :2009) whichever one was
    /// written, so an IEOS put also refreshes OEOS — pinned below.
    #[test]
    fn option_and_eos_puts_fall_through_to_a_driver_re_read() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        /// A driver whose hardware ignores what it is told: the baud set is
        /// accepted but the line stays at 9600, and both EOS sets are rejected
        /// outright while the driver keeps its own terminators.
        struct StubbornDriver(PortDriverBase);
        impl PortDriver for StubbornDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                _k: &str,
                _v: &str,
            ) -> crate::error::AsynResult<()> {
                Ok(())
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                match key {
                    "baud" => Ok("9600".to_string()),
                    "parity" => Ok("even".to_string()),
                    _ => Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unsupported option {key}"),
                    }),
                }
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "ieos boom".to_string(),
                })
            }
            fn get_input_eos(&self, _user: &AsynUser) -> Vec<u8> {
                b"\r\n".to_vec()
            }
            fn set_output_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "oeos boom".to_string(),
                })
            }
            fn get_output_eos(&self, _user: &AsynUser) -> Vec<u8> {
                b"\n".to_vec()
            }
        }

        let port_name = "test_option_eos_reread";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(StubbornDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();

        // The operator asks for 115200. The set succeeds; the driver stays at
        // 9600. C's getOptions fall-through snaps LBAUD (and the BAUD menu
        // index derived from it) back to what the port really runs at.
        rec.lbaud = 115_200;
        rec.special("LBAUD", true).unwrap();
        assert_eq!(rec.lbaud, 9600, "LBAUD is a driver readback, not a latch");
        assert_eq!(rec.baud, baud_choice_index("9600"));
        assert_eq!(rec.errs, "", "the set succeeded — no ERRS");
        // A field the driver refuses to report is left alone (C only overwrites
        // what `getOption` filled in).
        assert_eq!(rec.prty, 2, "parity readback: even");

        // The EOS set fails. C reports it, then re-reads BOTH EOS strings and
        // shows the driver's actual terminators — escaped, as `getEos` does
        // with epicsStrSnPrintEscaped.
        rec.ieos = "\\n".to_string();
        rec.special("IEOS", true).unwrap();
        assert_eq!(rec.errs, "Error setting input eos, ieos boom");
        assert_eq!(rec.ieos, "\\r\\n", "IEOS snaps back to the driver's EOS");
        assert_eq!(
            rec.oeos, "\\n",
            "an IEOS put re-reads the output EOS too (asynRecord.c:2009)"
        );
    }

    /// R9-50: C `performOctetIO` calls the pre-write flush as a bare statement
    /// and discards its status (asynRecord.c:1521) — unlike every other
    /// transfer in the routine, whose status is assigned and reported. So a
    /// flush failure never reaches ERRS. The port recorded `"flush: {e}"`,
    /// and since ERRS is only cleared at the top of `process()`, a *successful*
    /// Write/Read transaction still ended carrying the flush complaint.
    #[test]
    fn flush_failure_is_discarded_and_does_not_reach_errs() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        /// Flush fails; the write and the read that follow it succeed. C's
        /// Write/Read flushes first (:1518-1523), so this is the exact shape
        /// the finding is about.
        struct FlushFailsDriver(PortDriverBase);
        impl PortDriver for FlushFailsDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> crate::error::AsynResult<()> {
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "flush boom".into(),
                })
            }
            fn io_write_octet(
                &mut self,
                _user: &mut AsynUser,
                data: &[u8],
            ) -> crate::error::AsynResult<usize> {
                Ok(data.len())
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                buf[..2].copy_from_slice(b"OK");
                Ok((2, EomReason::END))
            }
        }

        let port_name = "test_flush_failure_discarded";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(FlushFailsDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
            ))),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::WriteRead as i32;
        rec.ofmt = ASYN_FMT_ASCII;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.aout = "CMD".to_string();
        rec.process().unwrap();

        assert_eq!(rec.ainp, "OK", "the write/read after the flush still ran");
        assert_eq!(
            rec.errs, "",
            "C discards the flush status; a good transaction reports nothing"
        );
        assert_eq!(read_alarm(&mut rec).1, AlarmSeverity::NoAlarm);

        // TMOD=Flush is a flush-only cycle: still nothing in ERRS.
        rec.tmod = TransferMode::Flush as i32;
        rec.process().unwrap();
        assert_eq!(rec.errs, "", "a flush-only cycle reports nothing either");
    }

    /// R8-56: the octet read-error ERRS is C's `"%s  nread %d %s"` with the
    /// status word ∈ {timeout, overflow, error}, the transferred count, and the
    /// driver's `pasynUser->errorMessage` (asynRecord.c:1593-1598 — it
    /// overwrites the earlier `"Error %s"` at :1583, which never reaches the
    /// operator). The port emitted `"read: {e}"`, which carries neither the
    /// status word nor the count and prefixes the Rust status debug onto the
    /// driver text.
    #[test]
    fn octet_read_error_errs_matches_c_report_error() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::{EomReason, PartialOctetRead};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        /// One read per status word C distinguishes; the third read carries a
        /// partial transfer so the `%d` count is not trivially zero.
        struct StatusDriver {
            base: PortDriverBase,
            n: usize,
        }
        impl PortDriver for StatusDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::error::AsynResult<(usize, EomReason)> {
                self.n += 1;
                match self.n {
                    1 => Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
                    }),
                    2 => Err(AsynError::Status {
                        status: AsynStatus::Overflow,
                        message: "buffer full".into(),
                    }),
                    _ => Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "device fault".into(),
                    }
                    .with_partial_read(PartialOctetRead {
                        data: b"xy".to_vec(),
                        eom_reason: EomReason::empty(),
                    })),
                }
            }
        }

        let port_name = "test_octet_read_errs_text";
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(StatusDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                n: 0,
            }),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.ifmt = ASYN_FMT_ASCII;

        rec.process().unwrap();
        assert_eq!(rec.errs, "timeout  nread 0 read timeout");

        rec.process().unwrap();
        assert_eq!(rec.errs, "overflow  nread 0 buffer full");

        rec.process().unwrap();
        assert_eq!(
            rec.errs, "error  nread 2 device fault",
            "the count is the bytes the failing read did deliver"
        );
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.iface = 0; // asynOctet
        rec.tmod = TransferMode::Read as i32;
        rec.imax = 256;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.process().unwrap();

        assert_eq!(rec.ainp, "abc", "the partial line reaches AINP (C :1503)");
        assert_eq!(rec.nord, 3, "NORD = nbytesTransfered (C :1627)");
        assert_eq!(rec.eomr, 0, "no EOS matched, buffer never filled (C :1591)");
        assert_eq!(rec.tinp, "abc", "TINP is posted for the failed read too");
        assert_eq!(
            rec.errs, "timeout  nread 3 read timeout",
            "C :1593-1598 \"%s  nread %d %s\" with the transferred count"
        );

        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::READ_ALARM, "C :1599 recGblSetSevr");
        assert_eq!(c.nsev, AlarmSeverity::Major, "READ_ALARM is MAJOR");

        // Binary IFMT takes the same transfer through BINP.
        let mut bin = AsynRecord::default();
        bin.port = port_name.to_string();
        let _ = bin.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
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

        // CNCT=1 on the disconnected port is REFUSED: the record's special
        // user rides the Connect queue at the record's ADDR (0) with no
        // sentinel, and C's `checkPortConnect` waiver covers only addr == -1
        // or the sentinel (asynManager.c:1520,1536-1538) — the W10-D1 wart,
        // reproduced by decision. The refusal is `queueRequest`'s return value,
        // so `asynCallbackSpecial` never runs: C reports
        // `pasynUserSpecial->errorMessage` and frees the user
        // (asynRecord.c:571-576), and with no callback there is no `monitorStatus`
        // tail — CNCT keeps the value the operator typed until the next status
        // cycle (R14-46).
        rec.cnct = 1;
        rec.special("CNCT", true).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            base_connects,
            "a device-addressed CNCT=1 put must be refused on a disconnected \
             port (C checkPortConnect)"
        );
        assert!(
            rec.errs.contains("not connected"),
            "the gate's refusal message is what reaches ERRS: {}",
            rec.errs
        );
        assert!(
            !rec.errs.contains("callbackConnect"),
            "a refused request never entered the callback: {}",
            rec.errs
        );
        assert_eq!(
            rec.cnct, 1,
            "no callback ran, so no monitorStatus tail snapped CNCT back \
             (asynRecord.c:571-576 vs :897)"
        );

        // The port-level route (C `connectDevice(port, -1)`) brings the line
        // back up; CNCT reads it back on the next status cycle.
        rec.port_entry
            .as_ref()
            .unwrap()
            .handle
            .connect_blocking()
            .unwrap();
        rec.cnct = 1;
        rec.special("CNCT", true).unwrap();
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

    /// R14-46: a request the queue gate refuses never ran, and the record must
    /// report it as a refusal — not as a callback that did something.
    ///
    /// C `special()` (asynRecord.c:571-576): when `queueRequest` returns non-zero
    /// the record writes `pasynUserSpecial->errorMessage` into ERRS and frees the
    /// user. `asynCallbackSpecial` is never dispatched, so none of the work it
    /// implies happens — no `setOption`, no `setEos`, no `connect`, no
    /// `getOptions`/`getEos` readback (the `/* no break */` fall-through lives
    /// *inside* the callback, :845-849) and no `monitorStatus` tail (:897).
    ///
    /// The same on the process side: C `process()` (:342-361) reports the literal
    /// `"queueRequest failed"` with `STATE_ALARM`/`MINOR_ALARM` and never enters
    /// `performIO`, so no transfer field is published.
    ///
    /// One case per boundary of "did the request run?": refused-disabled,
    /// refused-disconnected, refused-CNCT, refused-process — plus the control
    /// that must keep the callback tail, a driver error raised *inside* a
    /// callback that ran.
    #[test]
    fn a_gate_refused_request_reports_the_refusal_and_runs_no_callback() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::mpsc;

        #[derive(Default)]
        struct Log {
            option_sets: Vec<(String, String)>,
            eos_sets: Vec<Vec<u8>>,
            reads: usize,
        }

        struct GateDriver {
            base: PortDriverBase,
            log: Arc<Mutex<Log>>,
            connects: Arc<AtomicUsize>,
        }
        impl PortDriver for GateDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                key: &str,
                value: &str,
            ) -> crate::error::AsynResult<()> {
                self.log
                    .lock()
                    .unwrap()
                    .option_sets
                    .push((key.to_string(), value.to_string()));
                if value == "Unknown" {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("Bad {key}"),
                    });
                }
                Ok(())
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                match key {
                    "baud" => Ok("9600".to_string()),
                    _ => Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unsupported option {key}"),
                    }),
                }
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                self.log.lock().unwrap().eos_sets.push(eos.to_vec());
                Ok(())
            }
            fn read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::error::AsynResult<usize> {
                self.log.lock().unwrap().reads += 1;
                let data = b"hi";
                buf[..data.len()].copy_from_slice(data);
                Ok(data.len())
            }
            fn connect(&mut self, _user: &AsynUser) -> crate::error::AsynResult<()> {
                self.connects.fetch_add(1, Ordering::SeqCst);
                self.base.set_connected(true);
                Ok(())
            }
        }

        let port_name = "r14_46_gate_refusal";
        let log = Arc::new(Mutex::new(Log::default()));
        let connects = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(64);
        let actor = PortActor::new(
            Box::new(GateDriver {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                log: log.clone(),
                connects: connects.clone(),
            }),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        assert_eq!(rec.baud, baud_choice_index("9600"), "the port runs at 9600");
        let entry_handle = rec.port_entry.as_ref().unwrap().handle.clone();

        // --- Control: the callback RAN and the driver refused the value. C
        // reports "Error setting option, %s" (:1828-1830) *and* keeps the whole
        // callback tail — the getOptions fall-through snaps BAUD back to 9600.
        log.lock().unwrap().option_sets.clear();
        rec.baud = 0; // menu index 0 = "Unknown"
        rec.special("BAUD", true).unwrap();
        assert_eq!(
            log.lock().unwrap().option_sets,
            vec![("baud".to_string(), "Unknown".to_string())],
            "control: the request ran, so the driver saw the put"
        );
        assert_eq!(rec.errs, "Error setting option, Bad baud");
        assert_eq!(
            rec.baud,
            baud_choice_index("9600"),
            "control: the callback's readback tail ran"
        );

        // --- Boundary 1: refused because the port is DISABLED
        // (asynManager.c:1541-1546 — no waiver reaches it).
        entry_handle.set_enable_blocking(false).unwrap();
        log.lock().unwrap().option_sets.clear();
        rec.baud = baud_choice_index("19200");
        rec.special("BAUD", true).unwrap();
        assert!(
            log.lock().unwrap().option_sets.is_empty(),
            "a refused put must not reach the driver"
        );
        assert!(
            rec.errs.contains("disabled"),
            "ERRS is the gate's errorMessage (asynRecord.c:575): {}",
            rec.errs
        );
        assert_eq!(
            rec.baud,
            baud_choice_index("19200"),
            "no callback ran, so no getOptions readback snapped BAUD back"
        );

        // --- Boundary 2: refused process I/O on the same disabled port. C
        // reports "queueRequest failed" with STATE/MINOR and never enters
        // performIO (asynRecord.c:342-361).
        rec.tmod = TransferMode::Read as i32;
        rec.nord = 0;
        rec.process().unwrap();
        assert_eq!(
            log.lock().unwrap().reads,
            0,
            "a refused process request must not reach the driver"
        );
        assert_eq!(rec.errs, "queueRequest failed");
        assert_eq!(rec.nord, 0, "no transfer, so no NORD to publish");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::STATE_ALARM, AlarmSeverity::Minor),
            "C alarms the refusal STATE/MINOR (:361), not COMM/MAJOR"
        );

        // --- Boundary 3: refused because the port is DISCONNECTED
        // (checkPortConnect, asynManager.c:1547-1552) — an EOS put carries no
        // waiver, so it is refused with the port enabled but the line down.
        entry_handle.set_enable_blocking(true).unwrap();
        entry_handle.disconnect_blocking().unwrap();
        log.lock().unwrap().eos_sets.clear();
        rec.ieos = "\\n".to_string();
        rec.special("IEOS", true).unwrap();
        assert!(
            log.lock().unwrap().eos_sets.is_empty(),
            "a refused EOS put must not reach the driver"
        );
        assert!(
            rec.errs.contains("not connected"),
            "ERRS is the gate's errorMessage: {}",
            rec.errs
        );

        // --- Boundary 4: the CNCT put refused on that same disconnected port
        // (the device-addressed W10-D1 wart). No connect is attempted, and with
        // no callback there is no monitorStatus tail: CNCT keeps the operator's
        // value instead of being snapped back.
        let connects_before = connects.load(Ordering::SeqCst);
        rec.cnct = 1;
        rec.special("CNCT", true).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            connects_before,
            "a refused CNCT put must not touch the wire"
        );
        assert!(
            rec.errs.contains("not connected"),
            "ERRS is the gate's errorMessage, not a callback-shaped text: {}",
            rec.errs
        );
        assert_eq!(rec.cnct, 1, "no callback ran, so no monitorStatus tail");
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
            let actor_id = actor.id();
            std::thread::spawn(move || actor.run());
            let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
            register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            let _ = rec.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mk = |iface: i32, tmod: TransferMode| {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            let _ = rec.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();
        requested
    }

    /// R9-46: C `asynCallbackProcess` assigns TMOT to the asynUser verbatim —
    /// `pasynUser->timeout = pasynRec->tmot` (asynRecord.c:818) — and the value
    /// is three-valued: `>0` bounded wait, `==0` non-blocking poll
    /// (`drvAsynIPPort.c:741-742` floors the poll to 1 ms;
    /// `drvAsynSerialPort.c:902-905` sets `VMIN=0,VTIME=0`), `<0` wait forever.
    /// The port substituted a 1 s wait for BOTH non-positive cases, so an
    /// operator asking for a poll got a one-second block on a silent device.
    ///
    /// `tmot >= 0` now passes through verbatim. `tmot < 0` remains the bounded
    /// 1 s fallback: DRV-42 (`user.rs:15-22`) makes `AsynUser::timeout` an
    /// unsigned `Duration` on purpose — every blocking driver op is bounded —
    /// and that is a signed-off framework deviation, not something this record
    /// may override. Boundary-per-case, not scenario-per-case.
    #[test]
    fn tmot_is_passed_through_verbatim_when_non_negative() {
        use std::time::Duration;

        let plan_timeout = |tmot: f64| {
            let mut rec = AsynRecord::default();
            rec.tmot = tmot;
            rec.build_io_plan().timeout
        };

        // The defect: a zero TMOT is a poll, not a one-second wait.
        assert_eq!(
            plan_timeout(0.0),
            Duration::ZERO,
            "TMOT=0 is C's non-blocking poll"
        );
        // Positive values were already verbatim; pin them so the owner keeps them.
        assert_eq!(plan_timeout(2.5), Duration::from_millis(2500));
        assert_eq!(plan_timeout(0.001), Duration::from_millis(1));
        // DRV-42: "wait forever" is unrepresentable — bounded fallback.
        assert_eq!(
            plan_timeout(-1.0),
            Duration::from_secs(1),
            "negative TMOT falls back to the DRV-42 bounded default"
        );
        // Same fallback for values `Duration` cannot carry. C casts these with
        // `(int)`, which is undefined behaviour, so there is no C semantics here.
        assert_eq!(plan_timeout(f64::NAN), Duration::from_secs(1));
        assert_eq!(plan_timeout(f64::INFINITY), Duration::from_secs(1));
    }

    /// R9-46, end to end: the TMOT the operator set is the timeout the driver's
    /// `AsynUser` carries. Pins the value at the C boundary
    /// (`pasynUser->timeout`), not just inside the plan.
    #[test]
    fn tmot_zero_reaches_the_driver_as_a_zero_timeout() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct TimeoutSpy {
            base: PortDriverBase,
            seen: Arc<Mutex<Option<std::time::Duration>>>,
        }
        impl PortDriver for TimeoutSpy {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet(
                &mut self,
                user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::error::AsynResult<usize> {
                *self.seen.lock().unwrap() = Some(user.timeout);
                buf[0] = b'x';
                Ok(1)
            }
        }

        let port_name = "test_tmot_zero_to_driver";
        let seen = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(
            Box::new(TimeoutSpy {
                base: PortDriverBase::new(port_name, 1, PortFlags::default()),
                seen: seen.clone(),
            }),
            rx,
        );
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        let mut rec = read_rec(port_name, 0, 40, 0);
        rec.tmot = 0.0;
        rec.process().unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            Some(std::time::Duration::ZERO),
            "the driver's asynUser must carry the operator's TMOT=0 poll"
        );
    }

    fn read_rec(port_name: &str, ifmt: i32, imax: i32, nrrd: i32) -> AsynRecord {
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_id);
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();
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
        let _ = rec.connect_device();
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
        // R9-49 corrects this expectation: C `reportError`s the overflow
        // (asynRecord.c:1602-1608) *as well as* raising the MINOR alarm. The
        // previous `assert_eq!(rec.errs, "")` with the comment "overflow is not
        // an error, only a MINOR alarm" pinned behaviour the C reference does
        // not have — ERRS is the only place C tells the operator the response
        // was truncated. The trailing space is C's: the format is
        // "Overflow nread %d %s" and the `%s` (pasynUser->errorMessage) is empty
        // when the read itself did not fail.
        assert_eq!(rec.errs, "Overflow nread 40 ");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );
    }

    /// R9-49: every C overflow branch reports "Overflow nread %d %s"
    /// (asynRecord.c:1602-1615) — ASCII against `sizeof(ainp)`, Hybrid against
    /// IMAX — and the text overwrites the read-status text when the read both
    /// failed and overflowed (`reportError` `strncpy`s over ERRS, and the
    /// overflow check runs last). The port raised the MINOR alarm and left ERRS
    /// blank, so `process()`'s ERRS clear (:3043 equivalent) was all the
    /// operator saw.
    #[test]
    fn overflow_read_reports_errs_in_every_ifmt() {
        use crate::interpose::EomReason;
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::AlarmSeverity;

        // ASCII: 100 bytes offered, 40-byte AINP -> overflow at 40.
        let port = "test_overflow_errs_ascii";
        spawn_fill_port(port, 100, EomReason::CNT);
        let mut ascii = read_rec(port, ASYN_FMT_ASCII, 80, 0);
        ascii.process().unwrap();
        assert_eq!(ascii.errs, "Overflow nread 40 ");
        assert_eq!(
            read_alarm(&mut ascii),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );

        // Hybrid: the capacity is IMAX (asynRecord.c:1609-1615).
        let hport = "test_overflow_errs_hybrid";
        spawn_fill_port(hport, 100, EomReason::CNT);
        let mut hybrid = read_rec(hport, ASYN_FMT_HYBRID, 16, 0);
        hybrid.process().unwrap();
        assert_eq!(hybrid.errs, "Overflow nread 16 ");
        assert_eq!(
            read_alarm(&mut hybrid),
            (alarm_status::READ_ALARM, AlarmSeverity::Minor)
        );

        // A read that fits reports nothing.
        let sport = "test_overflow_errs_short";
        spawn_fill_port(sport, 8, EomReason::END);
        let mut short = read_rec(sport, ASYN_FMT_ASCII, 80, 0);
        short.process().unwrap();
        assert_eq!(short.errs, "", "a read inside the buffer reports nothing");
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
        // NOWT > OMAX is clamped by `clamp_transfer_sizes` (C asynRecord.c:1499
        // writes the clamp back into the field), which `build_io_plan` runs
        // before this; the payload is then simply the first NOWT bytes.
        rec.omax = 3;
        rec.nowt = 10;
        rec.clamp_transfer_sizes(AINP_SIZE);
        assert_eq!(rec.nowt, 3);
        assert_eq!(rec.octet_output_buffer(), vec![b'\\', b'r', 0x00]);
    }

    /// R18-83: C's `put_array_info` (asynRecord.c:983-993) sets NOWT from the
    /// element count of the BOUT put, and NORD from the count of a BINP put —
    /// `dbPut` calls it on every array put (`dbAccess.c:1366-1369`). Writing the
    /// array *is* how the count gets set; a client does not put NOWT alongside.
    ///
    /// Without it, a `caput -a` of 120 bytes into BOUT left NOWT at its default
    /// 80 and the record sent 80 bytes to the device — the wrong byte count on
    /// the wire, silently.
    #[test]
    fn an_array_put_sets_its_element_count() {
        let mut rec = AsynRecord::default();
        assert_eq!(rec.nowt, 80, "default NOWT (asynRecord.dbd)");

        let payload: Vec<u8> = (0..120u32).map(|i| (i % 251) as u8).collect();
        rec.omax = 1000;
        rec.ofmt = ASYN_FMT_BINARY;
        rec.put_field("BOUT", EpicsValue::CharArray(payload.clone()))
            .unwrap();

        assert_eq!(rec.nowt, 120, "BOUT put must set NOWT = nNew");
        assert_eq!(
            rec.octet_output_buffer(),
            payload,
            "every byte the client put must reach the device"
        );

        // The same C function serves BINP: its count is NORD.
        rec.put_field("BINP", EpicsValue::CharArray(vec![1, 2, 3]))
            .unwrap();
        assert_eq!(rec.nord, 3, "BINP put must set NORD = nNew");

        // A shorter put shrinks the count — nNew is the count that arrived, not
        // a high-water mark.
        rec.put_field("BOUT", EpicsValue::CharArray(vec![9, 9]))
            .unwrap();
        assert_eq!(rec.nowt, 2);
        assert_eq!(rec.octet_output_buffer(), vec![9, 9]);
    }

    /// The BOUT/BINP channel's native element count is the buffer capacity
    /// (OMAX/IMAX = 80 by default) — C `cvt_dbaddr` `no_elements = omax`/`imax`
    /// — distinct from the current transferred length the *value* carries.
    /// This is what `ca_element_count` reports; without it the channel
    /// advertised 0 (the empty buffer's length) where C advertises 80.
    #[test]
    fn bout_binp_native_count_is_the_buffer_capacity() {
        use epics_base_rs::server::record::Record;

        let rec = AsynRecord::default();
        // Capacity is OMAX/IMAX, served independent of the (empty) value.
        assert_eq!(rec.field_native_count("BOUT"), Some(80));
        assert_eq!(rec.field_native_count("BINP"), Some(80));
        // The value itself is still the current transferred bytes (empty here),
        // so channel count and value count are genuinely decoupled.
        assert_eq!(rec.get_field("BOUT"), Some(EpicsValue::CharArray(vec![])));
        assert_eq!(rec.get_field("BINP"), Some(EpicsValue::CharArray(vec![])));
        // A non-buffer field keeps the value's own count.
        assert_eq!(rec.field_native_count("AOUT"), None);
        assert_eq!(rec.field_native_count("PORT"), None);

        // Capacity tracks OMAX/IMAX, not a hardcoded 80.
        let mut rec = AsynRecord::default();
        rec.omax = 256;
        rec.imax = 512;
        assert_eq!(rec.field_native_count("BOUT"), Some(256));
        assert_eq!(rec.field_native_count("BINP"), Some(512));
    }

    /// R8-54: C `performOctetIO` writes the clamped transfer sizes back into
    /// the record — `nowt = omax` (asynRecord.c:1499) and `nrrd = inlen`
    /// (:1512) — and `monitor()` POST_IF_NEWs them (:1020,1022), so the fields
    /// show the *effective* sizes. The port used to clamp only into locals, so
    /// NOWT/NRRD kept the operator's over-large request forever.
    #[test]
    fn nowt_and_nrrd_are_clamped_back_into_the_record() {
        // Binary output: NOWT > OMAX clamps to OMAX.
        let mut rec = AsynRecord::default();
        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::WriteRead as i32;
        rec.ofmt = ASYN_FMT_BINARY;
        rec.ifmt = ASYN_FMT_BINARY;
        rec.omax = 8;
        rec.nowt = 500;
        rec.imax = 16;
        rec.nrrd = 900;
        rec.build_io_plan();
        assert_eq!(rec.nowt, 8, "NOWT clamps back to OMAX");
        assert_eq!(rec.nrrd, 16, "NRRD clamps back to IMAX for a Binary read");

        // ASCII input: the read capacity is sizeof(AINP), not IMAX
        // (asynRecord.c:1504-1505), so NRRD clamps to 40 even with IMAX=80.
        let mut ascii = AsynRecord::default();
        ascii.iface = InterfaceType::Octet as i32;
        ascii.tmod = TransferMode::Read as i32;
        ascii.ifmt = ASYN_FMT_ASCII;
        ascii.imax = 80;
        ascii.nrrd = 900;
        ascii.build_io_plan();
        assert_eq!(
            ascii.nrrd, AINP_SIZE as i32,
            "ASCII NRRD clamps to AINP_SIZE"
        );

        // A Read-only cycle still clamps NOWT, and a Write-only cycle still
        // clamps NRRD: C runs both clamps before any TMOD test.
        let mut read_only = AsynRecord::default();
        read_only.iface = InterfaceType::Octet as i32;
        read_only.tmod = TransferMode::Read as i32;
        read_only.ofmt = ASYN_FMT_BINARY;
        read_only.omax = 4;
        read_only.nowt = 99;
        read_only.build_io_plan();
        assert_eq!(read_only.nowt, 4, "TMOD=Read still clamps NOWT");

        // ASCII/Hybrid output is not length-bounded by NOWT in C, so the NOWT
        // clamp is Binary-only (asynRecord.c:1496-1501).
        let mut ascii_out = AsynRecord::default();
        ascii_out.iface = InterfaceType::Octet as i32;
        ascii_out.tmod = TransferMode::Write as i32;
        ascii_out.ofmt = ASYN_FMT_ASCII;
        ascii_out.omax = 4;
        ascii_out.nowt = 99;
        ascii_out.build_io_plan();
        assert_eq!(ascii_out.nowt, 99, "ASCII output leaves NOWT alone");

        // A register interface never reaches performOctetIO, so neither field
        // is touched (asynRecord.c:1326-1360).
        let mut i32_rec = AsynRecord::default();
        i32_rec.iface = InterfaceType::Int32 as i32;
        i32_rec.tmod = TransferMode::WriteRead as i32;
        i32_rec.ofmt = ASYN_FMT_BINARY;
        i32_rec.omax = 8;
        i32_rec.nowt = 500;
        i32_rec.imax = 16;
        i32_rec.nrrd = 900;
        i32_rec.build_io_plan();
        assert_eq!(i32_rec.nowt, 500, "Int32 interface leaves NOWT alone");
        assert_eq!(i32_rec.nrrd, 900, "Int32 interface leaves NRRD alone");
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
    fn canblock_int32_entry(value: i32) -> crate::registry::PortEntry {
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
        let actor_id = actor.id();
        std::thread::Builder::new()
            .name("asynio-test-actor".into())
            .spawn(move || actor.run())
            .unwrap();

        let mut handle = PortHandle::new(
            tx,
            "ASYNIO".into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        handle.set_can_block(true);
        crate::registry::PortEntry {
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
        let actor_id = actor.id();
        std::thread::Builder::new()
            .name("asynaqr-test-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let mut handle = PortHandle::new(
            tx,
            "ASYNAQR".into(),
            Arc::new(InterruptManager::new(16)),
            actor_id,
        );
        handle.set_can_block(true);
        let entry = crate::registry::PortEntry {
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );

        // A trace manager with an exception sink so trace changes announce
        // (without it, `exception_manager()` is None and no callback registers).
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Known baseline mask, then register the port for the record to find.
        trace.set_trace_mask(Some(port_name), TraceMask::empty());
        crate::registry::register_port(port_name, handle, trace.clone()).unwrap();

        // Build the record, hand it the database handle, and connect it —
        // connecting registers the trace exception callback with the same
        // async_ctx + runtime handle the framework supplies at add_record.
        let db = PvDatabase::new();
        let rec_name = "TRACE_IMM_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.to_string(), db.async_handle());
        let _ = rec.connect_device();
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
            let inst = db.get_record(rec_name).unwrap();
            let got = inst.read().record.get_field("TMSK");
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

        let inst = db.get_record(rec_name).unwrap();
        let g = inst.read();
        assert_eq!(g.record.get_field("TMSK"), Some(EpicsValue::Long(want)));
        assert_eq!(
            g.record.get_field("TB0"),
            Some(EpicsValue::Enum(1)),
            "ERROR bit posted"
        );
        assert_eq!(
            g.record.get_field("TB4"),
            Some(EpicsValue::Enum(1)),
            "FLOW bit posted"
        );
        assert_eq!(
            g.record.get_field("TB1"),
            Some(EpicsValue::Enum(0)),
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
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );

        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Baseline masks BEFORE connect so the callback seeds its last-posted
        // cache with these values (the C `old` after the connect-path
        // `monitorStatus`): TMSK = ERROR, all IO bits clear.
        trace.set_trace_mask(Some(port_name), TraceMask::ERROR);
        trace.set_trace_io_mask(Some(port_name), TraceIoMask::empty());
        crate::registry::register_port(port_name, handle, trace.clone()).unwrap();

        let db = PvDatabase::new();
        let rec_name = "TRACE_POSTIFNEW_REC";
        let rec = AsynRecord::default();
        db.add_record(rec_name, Box::new(rec)).await.unwrap();
        // Connect from *inside* the database — `add_record` is what hands a record
        // its db handle, so this is the order production runs in, and the only one
        // in which `connectDevice`'s own posts (R14-47) are observable at all.
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            // Seed the record's trace readbacks with the port's baseline, so the
            // attach itself posts no TMSK/TIOM: `connectDevice` POST_IF_NEWs what
            // it re-imports (R14-47), and an attach that *changed* TMSK would post
            // it — correctly, but that is the other test's subject. Here the port
            // must start out already showing what it will read back.
            rec.tmsk = TraceMask::ERROR.bits() as i32;
            rec.update_trace_bits_from_mask();
            rec.port = port_name.to_string();
            rec.special("PORT", true).unwrap();
            assert_eq!(rec.cnct, 1, "record must connect to the registered port");
        }

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

    /// R15-49: every ERRS write posts, and an unchanged one does not.
    ///
    /// C `reportError` (asynRecord.c:2028-2049) writes the field and then
    /// `db_post_events(errs, DBE_VALUE|DBE_LOG)` iff the text changed. ERRS is
    /// not `pp(TRUE)` (asynRecord.dbd:366-370), so nothing else posts it: the
    /// record's diagnostics — the queue-gate refusal, an option/EOS error, a
    /// connect error — were written into the field and never reached a CA client
    /// monitoring it. Only `resetError`'s clear posted.
    ///
    /// One case per boundary: a real refusal posts its text; the same text
    /// written again does not re-post; the clear posts.
    ///
    /// ERRS goes on the wire as a `DBF_CHAR` ARRAY, not a `DBF_STRING`:
    /// `cvt_dbaddr` (asynRecord.c:956-962) re-types it to
    /// `field_type = DBF_CHAR, no_elements = ERR_SIZE`, so a client sees the
    /// error text as bytes. The record stores it as a Rust `String`; the
    /// framework projects that onto the declared `DBF_CHAR` before serving it,
    /// which is what a monitor subscriber receives. Asserting
    /// `EpicsValue::String` here pinned the storage variant, i.e. the very
    /// type-from-the-value defect — the port served `DBF_STRING` where the C
    /// IOC serves `DBF_CHAR[ERR_SIZE]`.
    fn errs_on_the_wire(text: &str) -> EpicsValue {
        EpicsValue::CharArray(text.bytes().collect())
    }

    #[tokio::test]
    async fn every_errs_write_posts_and_an_unchanged_one_does_not() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::db_access::DbSubscription;
        use std::time::Duration;
        use tokio::sync::mpsc;

        struct DownDriver(PortDriverBase);
        impl PortDriver for DownDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        // A port whose link is down and will not come back: every option put the
        // record makes is refused by the queue gate ("port X not connected").
        let port_name = "test_errs_post";
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        base.init_connected(false);
        base.auto_connect = false;
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(DownDriver(base)), rx);
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        crate::registry::register_port(port_name, handle, trace).unwrap();

        let db = PvDatabase::new();
        let rec_name = "ERRS_POST_REC";
        db.add_record(rec_name, Box::new(AsynRecord::default()))
            .await
            .unwrap();
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            rec.port = port_name.to_string();
            rec.special("PORT", true).unwrap();
            rec.report_error(String::new()); // start from a clean, quiet ERRS
        }

        let mut errs_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.ERRS"))
            .await
            .expect("subscribe ERRS");

        // Boundary 1 — a refused put. C's `special()` writes the gate's own
        // `pasynUser->errorMessage` to ERRS and frees the user (:571-576); the
        // operator's ERRS monitor must fire with it.
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            rec.baud = 9600;
            rec.special("BAUD", true).unwrap();
            assert_eq!(
                rec.errs,
                format!("port {port_name} not connected"),
                "precondition: the queue gate refused the option put"
            );
        }
        assert_eq!(
            errs_sub.recv().await,
            Some(errs_on_the_wire(&format!("port {port_name} not connected"))),
            "a refused put must fire the ERRS monitor with the refusal text"
        );

        // Boundary 2 — the same text again. C guards the post on
        // `strncmp(errs, old.errs)` (:2044): a record retrying a down port must
        // not fire a monitor per retry with text the client already has.
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            rec.report_error(format!("port {port_name} not connected"));
        }
        let reposted = tokio::time::timeout(Duration::from_millis(300), errs_sub.recv()).await;
        assert!(
            reposted.is_err(),
            "an unchanged ERRS must not re-post, got {reposted:?}"
        );

        // Boundary 3 — the clear. C `resetError` (:2050-2060) posts the empty
        // string, so the operator's screen loses the stale message.
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            rec.reset_error();
        }
        assert_eq!(
            errs_sub.recv().await,
            Some(errs_on_the_wire("")),
            "resetError's clear must post"
        );
    }

    /// R14-47: `connectDevice` posts every readback it refreshes.
    ///
    /// C posts from three places inside the attach, and they are what put the new
    /// port's state on the operator's screen without a `process()`:
    /// `monitorStatus` (asynRecord.c:1270 and again at :1319) POST_IF_NEWs the
    /// trace masks, AUCT/CNCT/ENBL, PCNCT, REASON, DRVINFO and the interface
    /// flags; the queued `getOptions` callback posts BAUD…DRTO and HOSTINFO
    /// (:1926-1938); the queued `getEos` callback posts IEOS/OEOS (:2016-2024).
    ///
    /// The port refreshed all three sets and posted none of them: a PORT put
    /// silently re-pointed the record, and every readback field on the operator's
    /// screen kept the *previous* port's value until some other event fired a
    /// monitor.
    ///
    /// One case per posting site: an option field (BAUD), an EOS field (IEOS),
    /// and a monitorStatus field (PCNCT).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_device_posts_every_readback_it_refreshes() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::db_access::DbSubscription;
        use tokio::sync::mpsc;

        /// A serial-shaped port: 19200 baud and an input EOS the driver already
        /// holds (st.cmd's `asynOctetSetInputEos`).
        struct Serialish(PortDriverBase);
        impl PortDriver for Serialish {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                crate::interfaces::octet_transport_capabilities()
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                match key {
                    "baud" => Ok("19200".to_string()),
                    _ => Err(crate::error::AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unsupported option {key}"),
                    }),
                }
            }
        }

        let port_name = "r14_47_connect_posts";
        let mut drv = Serialish(PortDriverBase::new(port_name, 1, PortFlags::default()));
        drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
        let (tx, rx) = mpsc::channel(64);
        let actor = PortActor::new(Box::new(drv), rx);
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(64)),
            actor_id,
        );
        register_port(port_name, handle, Arc::new(TraceManager::new())).unwrap();

        // A record in the database with no port yet: BAUD/IEOS/PCNCT are at their
        // defaults, and the operator is watching them.
        let db = PvDatabase::new();
        let rec_name = "R14_47_REC";
        let rec = AsynRecord::default();
        db.add_record(rec_name, Box::new(rec)).await.unwrap();

        let mut baud_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.BAUD"))
            .await
            .expect("subscribe BAUD");
        let mut ieos_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.IEOS"))
            .await
            .expect("subscribe IEOS");
        let mut pcnct_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.PCNCT"))
            .await
            .expect("subscribe PCNCT");

        // The operator puts PORT — C `special()` :502-517 runs `connectDevice`.
        {
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            rec.port = port_name.to_string();
            rec.special("PORT", true).unwrap();
            assert_eq!(rec.baud, baud_choice_index("19200"), "the field refreshed");
            assert_eq!(rec.ieos, "\\r\\n", "…and the EOS with it");
            assert_eq!(rec.pcnct, 1, "…and the record attached");
        }

        // …and each of the three posting sites fired its monitor.
        assert_eq!(
            baud_sub.recv().await,
            Some(EpicsValue::Enum(baud_choice_index("19200") as u16)),
            "getOptions posts BAUD (asynRecord.c:1926)"
        );
        assert_eq!(
            ieos_sub.recv().await,
            Some(EpicsValue::String("\\r\\n".into())),
            "getEos posts IEOS (asynRecord.c:2016-2020)"
        );
        assert_eq!(
            pcnct_sub.recv().await,
            Some(EpicsValue::Enum(1)),
            "monitorStatus posts PCNCT (asynRecord.c:1128)"
        );
    }

    /// R8-55: C `exceptCallback` (asynRecord.c:903-917) takes EVERY
    /// `asynException` — its body is an unconditional `monitorStatus`, which
    /// re-reads `isAutoConnect` / `isConnected` / `isEnabled` (:1085-1099) and
    /// POST_IF_NEWs AUCT / CNCT / ENBL (:1125-1133). So a link that drops shows
    /// up in CNCT immediately, with no `process()` in between — which matters
    /// most for the passive record that never scans.
    ///
    /// The port subscribed the record to the three Trace* exceptions only and
    /// refreshed only the trace masks, so a disconnected port kept reporting
    /// CNCT="Connected" until something else processed the record.
    ///
    /// The disconnect is driven through the port actor, so the exception is
    /// announced *on the actor thread* (`PortDriver::disconnect` →
    /// `PortDriverBase::set_connected` → `announce_exception`). The refresh
    /// therefore must not use the `_blocking` queries: this test deadlocks if it
    /// does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn any_port_exception_refreshes_the_connect_state_immediately() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use tokio::sync::mpsc;

        struct PlainDriver(PortDriverBase);
        impl PortDriver for PlainDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_except_connect_state";
        // One exception manager, wired to BOTH the trace manager (so `setTrace*`
        // announces) and the driver (so connect/enable/auto-connect announce) —
        // the same wiring `PortManager::register_port` does.
        let exceptions = Arc::new(ExceptionManager::new());
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(exceptions.clone());

        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );
        crate::registry::register_port(port_name, handle.clone(), trace.clone()).unwrap();

        let db = PvDatabase::new();
        let rec_name = "EXCEPT_CNCT_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.to_string(), db.async_handle());
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1, "the port starts connected");
        assert_eq!(rec.enbl, 1, "the port starts enabled");
        db.add_record(rec_name, Box::new(rec)).await.unwrap();

        // Drop the link from a non-runtime thread — the iocsh / driver case.
        // The actor runs `disconnect`, which announces asynExceptionConnect from
        // inside the driver, on the actor's own thread.
        {
            let h = handle.clone();
            std::thread::spawn(move || h.disconnect_blocking().unwrap())
                .join()
                .unwrap();
        }

        let mut posted = false;
        for _ in 0..2000 {
            let inst = db.get_record(rec_name).unwrap();
            let got = inst.read().record.get_field("CNCT");
            if got == Some(EpicsValue::Enum(0)) {
                posted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            posted,
            "a disconnect must refresh CNCT immediately, with no process()"
        );

        // ENBL / AUCT are re-read by the same refresh and are unchanged, so
        // POST_IF_NEW leaves them alone — the record still shows them true.
        let inst = db.get_record(rec_name).unwrap();
        let g = inst.read();
        assert_eq!(g.record.get_field("ENBL"), Some(EpicsValue::Enum(1)));
        assert_eq!(g.record.get_field("AUCT"), Some(EpicsValue::Enum(1)));
    }

    /// R8-55, the no-database fallback boundary: a record with no `async_ctx`
    /// (connected outside a database, as in these unit tests) cannot post out of
    /// band, so the exception raises `status_dirty` and the next `process()`
    /// drains it through the single `monitor_status` owner. Before the fix the
    /// flag was raised only by the three Trace* exceptions, so a disconnect
    /// never reached CNCT at all — not even on the next process cycle.
    #[test]
    fn a_port_exception_refreshes_the_connect_state_on_the_next_process() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct PlainDriver(PortDriverBase);
        impl PortDriver for PlainDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_except_connect_state_dirty";
        let exceptions = Arc::new(ExceptionManager::new());
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(exceptions.clone());

        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_id = actor.id();
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new(256)),
            actor_id,
        );
        register_port(port_name, handle.clone(), trace).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);

        handle.disconnect_blocking().unwrap();
        // The exception has been announced; the record has no database handle,
        // so the refresh is deferred to the next cycle.
        rec.process().unwrap();
        assert_eq!(
            rec.cnct, 0,
            "the dropped link reaches CNCT on the next scan"
        );
        assert_eq!(rec.enbl, 1, "ENBL is re-read and unchanged");
        assert_eq!(rec.auct, 1, "AUCT is re-read and unchanged");
    }

    /// R10-55. A GPIB driver takes its interfaces from
    /// `pasynGpib->registerPort` (asynGpib.c:562-631) — asynCommon, asynOctet,
    /// asynGpib and asynInt32 — so on a vxi11 (drvVxi11.c:1761) or Prologix
    /// (drvPrologixGPIB.c:592) port C's `connectDevice` reads back GPIBIV = 1
    /// and I32IV = 1. vxi11 registers asynOption on top (drvVxi11.c:1777);
    /// Prologix does not.
    #[test]
    fn a_gpib_port_reads_back_gpibiv_and_i32iv() {
        use crate::drivers::prologix::DrvAsynPrologixPort;
        use crate::drivers::vxi11::DrvVxi11Port;
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        let vxi_name = "r10_55_vxi11";
        let (vxi_rt, _vxi_jh) = create_port_runtime(
            DrvVxi11Port::configure(vxi_name, "192.0.2.1", 0, "", "gpib0", 0, true).unwrap(),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            vxi_name,
            vxi_rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = vxi_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.gpibiv, 1, "asynGpib (C :1228-1234)");
        assert_eq!(rec.i32iv, 1, "asynInt32, registered by asynGpib (C :140)");
        assert_eq!(rec.octetiv, 1, "asynOctet");
        assert_eq!(rec.optioniv, 1, "vxi11 registers asynOption (:1777)");

        let prologix_name = "r10_55_prologix";
        let (p_rt, _p_jh) = create_port_runtime(
            DrvAsynPrologixPort::new(prologix_name, "192.0.2.1:1234", true).unwrap(),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            prologix_name,
            p_rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = prologix_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.gpibiv, 1, "asynGpib (C :1228-1234)");
        assert_eq!(rec.i32iv, 1, "asynInt32, registered by asynGpib (C :140)");
        assert_eq!(rec.octetiv, 1, "asynOctet");
        assert_eq!(
            rec.optioniv, 0,
            "drvPrologixGPIB registers no asynOption (:592)"
        );
    }

    /// R9-53. C `connectDevice` asks the manager for each interface in turn
    /// (`findInterface(asynOctetType / asynInt32Type / ... )`,
    /// asynRecord.c:1177-1240) and records what it found; `performIO` then
    /// refuses a transfer through an interface the port does not implement
    /// (:1328-1360). The port hardcoded OCTETIV/I32IV/UI32IV/F64IV/OPTIONIV = 1,
    /// so a pure-octet transport advertised register interfaces it never had and
    /// a Read with IFACE=asynInt32 was dispatched at a driver with no Int32
    /// support instead of being refused.
    #[test]
    fn connect_device_reads_the_ports_interface_registry() {
        use crate::interfaces::octet_transport_capabilities;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        // A pure-octet transport: an IP socket, a serial line. asynOctet and
        // asynOption, no register interfaces.
        struct OctetTransport(PortDriverBase);
        impl PortDriver for OctetTransport {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                octet_transport_capabilities()
            }
        }

        let port_name = "r9_53_octet_transport";
        let (rt, _jh) = create_port_runtime(
            OctetTransport(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();

        assert_eq!(rec.octetiv, 1, "the transport carries asynOctet");
        assert_eq!(rec.optioniv, 1, "and asynOption");
        assert_eq!(rec.i32iv, 0, "but no asynInt32 (C :1204-1210)");
        assert_eq!(rec.ui32iv, 0, "no asynUInt32Digital (C :1212-1218)");
        assert_eq!(rec.f64iv, 0, "no asynFloat64 (C :1220-1226)");
        assert_eq!(rec.gpibiv, 0, "no asynGpib (C :1228-1234)");

        // performIO refuses the interface the port does not implement, with
        // COMM_ALARM/MAJOR_ALARM and no I/O (C :1345-1348).
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.process().unwrap();
        assert_eq!(rec.errs, "No asynInt32 interface");
        assert_eq!(rec.i32inp, 0, "no read ran");
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);

        // The interface it does implement still runs.
        rec.iface = InterfaceType::Octet as i32;
        rec.errs.clear();
        rec.imax = 16;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.process().unwrap();
        assert_ne!(
            rec.errs, "No asynOctet interface",
            "asynOctet is implemented and must not be refused"
        );
    }

    /// R9-55. C queues the option callback on a `duplicateAsynUser` of the
    /// record's own `pasynUser` (asynRecord.c:531-533), and `duplicateAsynUser`
    /// copies `timeout` (asynManager.c:1225), which the record set to TMOT
    /// (asynRecord.c:818). `setOption` then gets that user (:1787-1826) and an RFC
    /// 2217 negotiation runs under it (asynInterposeCom.c:475,495). The port's
    /// option path carried no asynUser at all, so the record's TMOT never reached
    /// the driver and the negotiation used a private 2 s.
    #[test]
    fn an_option_put_reaches_the_driver_with_the_records_tmot() {
        use crate::interfaces::{Capability, octet_transport_capabilities};
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};
        use std::sync::Mutex as StdMutex;
        use std::time::Duration;

        static SEEN: StdMutex<Option<Duration>> = StdMutex::new(None);

        struct OptionSpy(PortDriverBase);
        impl PortDriver for OptionSpy {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                octet_transport_capabilities()
            }
            fn set_option(
                &mut self,
                user: &mut AsynUser,
                _key: &str,
                _value: &str,
            ) -> crate::error::AsynResult<()> {
                *SEEN.lock().unwrap() = Some(user.timeout);
                Ok(())
            }
        }

        let port_name = "r9_55_option_user";
        let (rt, _jh) = create_port_runtime(
            OptionSpy(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();

        rec.tmot = 3.5;
        rec.lbaud = 19200;
        rec.special("LBAUD", true).unwrap();

        assert_eq!(
            SEEN.lock().unwrap().take(),
            Some(Duration::from_millis(3500)),
            "the driver's setOption runs under the record's TMOT"
        );
    }

    /// R9-53, the interfaces with no IFACE menu entry: `setOption` and `setEos`
    /// take the same refusal from the same owner. C `setOption` on `!optioniv`
    /// (asynRecord.c:1766-1771) and `setEos` on `!octetiv` (:1956-1961) report
    /// "No asyn<X> interface" and raise COMM_ALARM/MAJOR_ALARM without touching
    /// the driver.
    #[test]
    fn option_and_eos_are_refused_on_a_port_that_lacks_the_interface() {
        use crate::interfaces::Capability;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        // A GPIB-style octet port: asynOctet only — no asynOption (the Prologix
        // driver's declaration).
        struct OctetNoOption(PortDriverBase);
        impl PortDriver for OctetNoOption {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                vec![Capability::OctetRead, Capability::OctetWrite]
            }
        }

        let port_name = "r9_53_no_option";
        let (rt, _jh) = create_port_runtime(
            OctetNoOption(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.optioniv, 0, "no asynOption interface");
        assert_eq!(rec.octetiv, 1);

        rec.lbaud = 9600;
        rec.special("LBAUD", true).unwrap();
        assert_eq!(rec.errs, "No asynOption interface");
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);

        // asynOctet is implemented, so the EOS put reaches the driver.
        rec.errs.clear();
        rec.ieos = "\\n".to_string();
        rec.special("IEOS", true).unwrap();
        assert_ne!(rec.errs, "No asynOctet interface");
    }

    // ===== R10-49: the record's QUEUE_TIMEOUT =====

    /// R10-49. C `asynRecord` is the one caller in asyn that asks `queueRequest`
    /// for a queue-wait deadline, and it asks for the same one everywhere:
    /// `QUEUE_TIMEOUT` = 10.0 s (asynRecord.c:71) on the process I/O (:343), on
    /// the special option/EOS callback (:572) and on the getOption/getEos
    /// readbacks `connectDevice` queues (:1281,:1297). The port armed no queue
    /// deadline at all.
    #[test]
    fn every_record_request_carries_c_queue_timeout() {
        use std::time::Duration;

        assert_eq!(
            QUEUE_TIMEOUT,
            Duration::from_secs(10),
            "C `#define QUEUE_TIMEOUT 10.0`"
        );

        let mut rec = AsynRecord::default();
        let plan = rec.build_io_plan();
        assert_eq!(io_user(&plan).queue_timeout, Some(QUEUE_TIMEOUT));
        assert_eq!(flush_user(&plan).queue_timeout, Some(QUEUE_TIMEOUT));
        assert_eq!(rec.option_user().queue_timeout, Some(QUEUE_TIMEOUT));

        // Negative control: a user built anywhere else (device support, iocsh,
        // a driver's own request) arms no timer — C's `queueRequest(..., 0.0)`.
        assert_eq!(AsynUser::default().queue_timeout, None);
        assert_eq!(AsynUser::new(3).with_addr(1).queue_timeout, None);
    }

    /// R10-49. C `queueTimeoutCallbackProcess` (asynRecord.c:919-926): the
    /// process request that never ran reports "process queueRequest timeout",
    /// raises STATE_ALARM/MAJOR_ALARM and forces the record's completion. It is
    /// **not** an I/O result: no transfer happened, so no NAWT/NORD/EOMR is
    /// published and the rest of the cycle's phases do not run.
    #[test]
    fn a_process_queue_timeout_reports_the_c_text_and_state_major() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::WriteRead as i32;
        let plan = rec.build_io_plan();

        let mut out = IoOutcome::default();
        let flow = record_phase_result(
            &plan,
            &mut out,
            IoPhase::Write,
            Err(crate::error::AsynError::QueueTimeout { port: "p".into() }),
        );
        assert!(
            matches!(flow, PhaseFlow::Aborted),
            "the request never ran: C never entered performIO, so no read follows"
        );
        assert_eq!(out.errs.as_deref(), Some("process queueRequest timeout"));
        assert_eq!(
            out.alarm,
            Some((alarm_status::STATE_ALARM, AlarmSeverity::Major))
        );
        assert_eq!(out.nawt, None, "nothing was written — nothing to publish");

        // The same on a read phase, and through the flush phase too — whose
        // recorder discards every *I/O* status (C :1521) but must not swallow a
        // request that never ran.
        for phase in [IoPhase::Read, IoPhase::Flush] {
            let mut out = IoOutcome::default();
            let flow = record_phase_result(
                &plan,
                &mut out,
                phase,
                Err(crate::error::AsynError::QueueTimeout { port: "p".into() }),
            );
            assert!(matches!(flow, PhaseFlow::Aborted), "{phase:?}");
            assert_eq!(out.errs.as_deref(), Some("process queueRequest timeout"));
            assert_eq!(
                out.alarm,
                Some((alarm_status::STATE_ALARM, AlarmSeverity::Major)),
                "{phase:?}"
            );
        }
    }

    /// Negative control for the gate above: an ordinary I/O failure is a *result*
    /// — the request ran, the device did not answer. It reports the driver's text
    /// with the read-error severity, and the cycle carries on (C `performIO` runs
    /// the read after a failed write).
    #[test]
    fn an_io_timeout_is_not_a_queue_timeout() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::WriteRead as i32;
        let plan = rec.build_io_plan();

        let mut out = IoOutcome::default();
        let flow = record_phase_result(
            &plan,
            &mut out,
            IoPhase::Read,
            Err(crate::error::AsynError::Status {
                status: AsynStatus::Timeout,
                message: "no response".into(),
            }),
        );
        assert!(
            matches!(flow, PhaseFlow::Continue),
            "a phase that ran and failed does not abort the cycle"
        );
        let errs = out.errs.clone().unwrap();
        assert!(
            errs.contains("timeout") && !errs.contains("queueRequest"),
            "the driver's read-error text, not the queue timeout's: {errs}"
        );
    }

    /// R10-49. C `queueTimeoutCallbackSpecial` (asynRecord.c:929-938) reports
    /// "special queueRequest timeout", returns the record to `stateIdle` and
    /// frees the request — and raises **no** severity, unlike its process twin.
    #[test]
    fn a_special_queue_timeout_reports_the_c_text_and_no_severity() {
        let mut rec = AsynRecord::default();
        rec.report_special_queue_timeout();
        assert_eq!(rec.errs, "special queueRequest timeout");

        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(
            c.nsev,
            AlarmSeverity::NoAlarm,
            "C raises no recGblSetSevr in queueTimeoutCallbackSpecial"
        );
    }

    // ===== R10-52: the option readback follows C's buffer rule =====

    /// Register a port whose `getOption("baud")` answers `baud_text` and which
    /// refuses every other key — a driver that reports its rate as free text, and
    /// the ordinary case of a port that does not implement a key at all.
    fn register_baud_text_port(port_name: &'static str, baud_text: &'static str) {
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct BaudTextPort(PortDriverBase, &'static str);
        impl PortDriver for BaudTextPort {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn get_option(&self, key: &str) -> crate::error::AsynResult<String> {
                if key == "baud" {
                    Ok(self.1.to_string())
                } else {
                    Err(crate::error::AsynError::OptionNotFound(key.to_string()))
                }
            }
        }

        let (rt, jh) = create_port_runtime(
            BaudTextPort(
                PortDriverBase::new(port_name, 1, PortFlags::default()),
                baud_text,
            ),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        std::mem::forget(rt);
    }

    /// R10-52. C reads LBAUD with `sscanf(optbuff, "%d", &pasynRec->lbaud)`
    /// (asynRecord.c:1867) — and `sscanf` writes *nothing* when the text carries
    /// no number, so LBAUD is the one readback field that keeps its previous
    /// value. The port's `parse::<i32>().unwrap_or(0)` wrote 0 instead, reporting
    /// a live line as 0 baud.
    #[test]
    fn a_baud_readback_with_no_number_leaves_lbaud_alone() {
        let port_name = "r10_52_no_number";
        register_baud_text_port(port_name, "unknown");

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.lbaud = 115200;
        // A field C *does* pre-zero, seeded with a stale value from a previous
        // port: it must not survive a readback the driver cannot answer.
        rec.dbit = 4;

        rec.connect_device().unwrap();

        assert_eq!(
            rec.lbaud, 115200,
            "C's sscanf leaves LBAUD untouched when the text carries no number"
        );
        assert_eq!(rec.baud, 0, "BAUD still lands on its Unknown choice");
        assert_eq!(
            rec.dbit, 0,
            "a key the port refuses is an empty readback, and C zeroes the enum before the choice walk"
        );
    }

    /// R10-52 negative control: a text that *does* carry a number still reaches
    /// LBAUD — through C's `%d`, which is a prefix parse, so a driver answering
    /// "9600 baud" reads 9600 where `str::parse` refused the whole string.
    #[test]
    fn a_baud_readback_with_a_number_reaches_lbaud() {
        let port_name = "r10_52_number";
        register_baud_text_port(port_name, "9600 baud");

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.lbaud = 115200;

        rec.connect_device().unwrap();

        assert_eq!(rec.lbaud, 9600);
        assert_eq!(
            rec.baud, 0,
            "BAUD matches the choice *text*, which \"9600 baud\" is not"
        );
    }

    // ===== R11-48: a port with no asynDrvUser interface =====

    /// Register an octet transport (asynOctet + asynOption + asynCommon — no
    /// asynDrvUser, exactly what drvAsynIPPort / drvAsynSerialPort register).
    fn register_octet_transport(port_name: &'static str) {
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct OctetTransport(PortDriverBase);
        impl PortDriver for OctetTransport {
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

        let (rt, jh) = create_port_runtime(
            OctetTransport(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        std::mem::forget(rt);
    }

    /// Register a parameter-library port (C `asynPortDriver`: it registers
    /// asynDrvUser and can resolve a drvInfo string).
    fn register_param_port(port_name: &'static str, param: &str) {
        use crate::param::ParamType;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct ParamDriver(PortDriverBase);
        impl PortDriver for ParamDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        // Reason 0 is taken by a filler param so the resolved reason is non-zero
        // and cannot be confused with the forced-zero case.
        base.params
            .create_param("FILLER", ParamType::Int32)
            .unwrap();
        base.params.create_param(param, ParamType::Int32).unwrap();
        let (rt, jh) = create_port_runtime(ParamDriver(base), RuntimeConfig::default())
            .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        std::mem::forget(rt);
    }

    /// R11-48. C `connectDevice` asks for asynDrvUser (asynRecord.c:1243); a byte
    /// transport registers none, so C forces `pasynRec->reason = 0`
    /// (asynRecord.c:1261) whatever the operator (or a save/restore file) left in
    /// the field, and reports a non-blank DRVINFO as a configuration error
    /// (:1263-1265). The port used to call `drvUser->create` on every port and
    /// read the resulting `ParamNotFound` as C's *create failed* case, so REASON
    /// survived the connect and ERRS carried the wrong text.
    #[test]
    fn a_port_without_asyn_drv_user_forces_reason_zero_and_reports_drvinfo() {
        let port_name = "r11_48_no_drvuser";
        register_octet_transport(port_name);

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        // What a save/restore file restores into a record pointed at a transport.
        rec.reason = 7;
        rec.drvinfo = "SOME_PARAM".to_string();

        rec.connect_device().unwrap();

        assert_eq!(
            rec.reason, 0,
            "C zeroes REASON on a port with no asynDrvUser"
        );
        assert_eq!(
            rec.resolved_reason, 0,
            "and no I/O may carry the stale reason"
        );
        assert_eq!(
            rec.errs, "asynDrvUser not supported but drvInfo not blank",
            "C reportError text (asynRecord.c:1264-1265)"
        );
    }

    /// W10-D3: `connectDevice` reads the EOS back from the driver.
    ///
    /// C queues a `callbackGetEos` whenever the port has asynOctet
    /// (asynRecord.c:1289-1300), right after the option readback, so a fresh
    /// record shows the EOS the *driver* holds. The port queued only the option
    /// readback, so IEOS/OEOS stayed blank until an operator wrote one — a record
    /// on a port whose driver was configured with `asynOctetSetInputEos` in
    /// st.cmd showed nothing.
    ///
    /// C's asymmetry with the option readback is real and is asserted here too:
    /// `callbackGetEos` is queued at `asynQueuePriorityLow` with no
    /// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (:1296), so on a *disconnected*
    /// port the queue gate refuses it (asynManager.c:1547-1552) and the fields
    /// stay blank, while the option readback — which does carry the waiver
    /// (:1277-1280) — still fills in.
    #[test]
    fn connect_device_reads_the_eos_back_from_the_driver() {
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct EosPort(PortDriverBase);
        impl PortDriver for EosPort {
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

        for (port_name, connected) in [("w10_d3_eos_up", true), ("w10_d3_eos_down", false)] {
            let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
            base.auto_connect = false;
            base.init_connected(connected);
            // The driver already holds an EOS — st.cmd's `asynOctetSetInputEos`.
            let mut drv = EosPort(base);
            drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
            drv.set_output_eos(&AsynUser::default(), b"\n").unwrap();
            let (rt, jh) = create_port_runtime(drv, RuntimeConfig::default())
                .expect("the port runtime thread must start");
            std::mem::forget(jh);
            register_port(
                port_name,
                rt.port_handle().clone(),
                Arc::new(TraceManager::new()),
            )
            .unwrap();
            std::mem::forget(rt);

            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            rec.connect_device().unwrap();

            if connected {
                assert_eq!(
                    rec.ieos, "\\r\\n",
                    "connectDevice queues callbackGetEos (asynRecord.c:1289-1300)"
                );
                assert_eq!(rec.oeos, "\\n");
            } else {
                // The Low-priority readback carries no waiver: C's gate refuses it.
                assert_eq!(
                    rec.ieos, "",
                    "a disconnected port's EOS readback is refused (asynRecord.c:1296)"
                );
                assert_eq!(rec.oeos, "");
            }
        }
    }

    /// W10-D2: every `asynCallbackSpecial` arm ends in `monitorStatus`
    /// (asynRecord.c:897) — the option, EOS and CNCT arms too, not just REASON.
    ///
    /// `monitorStatus` re-imports the status readbacks from the port and
    /// POST_IF_NEWs them: ENBL from `isEnabled` (:1094-1097), AUCT from
    /// `isAutoConnect` (:1084-1088), CNCT from `isConnected` (:1089-1093). The
    /// arms ran only their own narrow readback (option fields, EOS fields, CNCT
    /// alone), so a port whose state moved underneath the record kept showing the
    /// stale value — and after a *failed* CNCT put the snap-back fired no monitor,
    /// leaving the operator's screen on the value they typed.
    ///
    /// Each arm is checked against a port that was disabled and disconnected
    /// behind the record's back, so a missing `monitorStatus` is visible as a
    /// stale ENBL/CNCT.
    #[test]
    fn every_special_callback_arm_ends_in_monitor_status() {
        for (field, port_name) in [
            ("BAUD", "w10_d2_option"),
            ("IEOS", "w10_d2_eos"),
            ("CNCT", "w10_d2_cnct"),
        ] {
            register_octet_transport(port_name);

            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            rec.connect_device().unwrap();
            assert_eq!(rec.enbl, 1, "{field}: the port starts enabled");
            assert_eq!(rec.cnct, 1, "{field}: …and connected");
            assert_eq!(rec.auct, 1, "{field}: …and auto-connecting");

            // The port moves underneath the record — another IOC thread, an
            // `asynAutoConnect` from the shell. Nothing has told the record.
            //
            // The move has to be one the queue gate tolerates: a *disabled* or
            // *disconnected* port refuses the put at `queueRequest`, and then C
            // runs no callback and therefore no `monitorStatus` either
            // (asynRecord.c:571-576 — that is R14-46's boundary, asserted in
            // `a_gate_refused_request_reports_the_refusal_and_runs_no_callback`).
            // What this test is about is the *other* side of that line: a request
            // that DID run must end in the tail.
            let handle = rec.port_entry.as_ref().unwrap().handle.clone();
            handle.set_auto_connect_blocking(false).unwrap();

            // Now the operator puts the field. Whatever the arm does with it, C's
            // callback ends in `monitorStatus`.
            rec.special(field, true).unwrap();

            assert_eq!(
                rec.auct, 0,
                "{field}: monitorStatus re-imports AUCT from isAutoConnect \
                 (asynRecord.c:1084-1088) at the tail of every arm (:897)"
            );
        }
    }

    /// R11-C8: a REASON put blanks DRVINFO — and that is what makes the put
    /// stick.
    ///
    /// C's `special()` REASON arm runs four statements (asynRecord.c:487-492):
    /// assign `pasynUser->reason`, `strcpy(pasynRec->drvinfo, "")`,
    /// `cancelIOInterruptScan`, `monitorStatus`. The blank is load-bearing:
    /// DRVINFO is the *other* way to set REASON, and `connectDevice` re-resolves
    /// it through `asynDrvUser->create` and assigns the result over REASON
    /// whenever it is non-empty (:1248-1254). Keeping the stale string meant the
    /// next reconnect silently undid the operator's put.
    #[test]
    fn a_reason_put_blanks_drvinfo_so_a_reconnect_cannot_undo_it() {
        let port_name = "r11_c8_reason_put";
        register_param_port(port_name, "GAIN");

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "GAIN".to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.reason, 1, "DRVINFO resolved GAIN to parameter 1");
        assert_eq!(rec.resolved_reason, 1);

        // The operator overrides REASON by hand.
        rec.reason = 3;
        rec.special("REASON", true).unwrap();
        assert_eq!(
            rec.resolved_reason, 3,
            "C assigns pasynUser->reason from the field (asynRecord.c:488)"
        );
        assert_eq!(
            rec.drvinfo, "",
            "C blanks DRVINFO in the same arm (asynRecord.c:489)"
        );

        // The reconnect any PORT/ADDR put, PCNCT=1 or IOC restart performs must
        // now leave the operator's REASON alone: there is no DRVINFO left to
        // re-resolve it from.
        rec.connect_device().unwrap();
        assert_eq!(
            rec.reason, 3,
            "a stale DRVINFO would have re-resolved REASON back to 1"
        );
        assert_eq!(rec.resolved_reason, 3, "and the I/O user with it");
    }

    /// R11-48: the zeroing is unconditional — C runs `pasynRec->reason = 0` above
    /// the DRVINFO test, so an empty DRVINFO zeroes REASON too and leaves ERRS
    /// clean.
    #[test]
    fn a_port_without_asyn_drv_user_zeroes_reason_even_with_blank_drvinfo() {
        let port_name = "r11_48_no_drvuser_blank";
        register_octet_transport(port_name);

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.reason = 7;

        rec.connect_device().unwrap();

        assert_eq!(rec.reason, 0);
        assert_eq!(rec.resolved_reason, 0);
        assert_eq!(rec.errs, "", "a blank DRVINFO is not an error");
    }

    /// R11-48 negative control: a port that DOES register asynDrvUser still
    /// resolves DRVINFO through `drvUser->create` and still reports C's create
    /// failure text for a name the driver rejects — the new branch must not
    /// swallow either case.
    #[test]
    fn a_port_with_asyn_drv_user_still_resolves_and_still_reports_create_failure() {
        let port_name = "r11_48_drvuser";
        register_param_port(port_name, "GAIN");

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "GAIN".to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.reason, 1, "DRVINFO resolved through the driver");
        assert_eq!(rec.resolved_reason, 1);
        assert_eq!(rec.errs, "");

        // A name the driver does not know: C's create failure, not the
        // no-interface report.
        rec.drvinfo = "NO_SUCH_PARAM".to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.errs, "Error in asynDrvUser->create()");
        assert_eq!(rec.resolved_reason, 0);

        // A blank DRVINFO on such a port keeps the operator's REASON — C never
        // zeroes it on the asynDrvUser branch (asynRecord.c:1244-1257).
        rec.drvinfo.clear();
        rec.reason = 5;
        rec.connect_device().unwrap();
        assert_eq!(rec.reason, 5);
        assert_eq!(rec.resolved_reason, 5);
        assert_eq!(rec.errs, "");
    }

    // ===== R11-46: SCAN="I/O Intr" =====

    /// A port whose reads are countable, so a process cycle that performed I/O is
    /// distinguishable from one a driver interrupt drove.
    struct IoIntrDriver {
        base: PortDriverBase,
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PortDriver for IoIntrDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        }
    }

    /// Register a port that counts its Int32 reads. Returns its runtime handle
    /// (kept alive by the caller) and the read counter.
    fn io_intr_port(
        name: &str,
    ) -> (
        crate::runtime::PortRuntimeHandle,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use crate::port::PortFlags;
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (rt, _jh) = create_port_runtime(
            IoIntrDriver {
                base: PortDriverBase::new(name, 1, PortFlags::default()),
                reads: reads.clone(),
            },
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        (rt, reads)
    }

    /// A driver interrupt, as `notify_interface_value` fires it: addr 0, typed
    /// for the interface the record selected.
    fn fire_interrupt(
        rt: &crate::runtime::PortRuntimeHandle,
        reason: usize,
        value: crate::param::ParamValue,
        iface: crate::interfaces::InterfaceType,
        changed_mask: u32,
    ) {
        rt.port_handle()
            .interrupts()
            .notify(crate::interrupt::InterruptValue {
                reason,
                addr: 0,
                value,
                uint32_changed_mask: changed_mask,
                iface: Some(iface),
                ..Default::default()
            });
    }

    /// R11-46. C's I/O Intr mode: `getIoIntInfo` registers the driver callbacks
    /// (asynRecord.c:582-597), `callbackInterruptInt32` stores the pushed value in
    /// I32INP and sets `gotValue` (:747-751), and `process` skips **all** I/O for
    /// the cycle that value drove (`if (gotValue) goto done`, :341). The port had
    /// no I/O Intr mechanism at all: a record on `SCAN="I/O Intr"` was demoted to
    /// Passive at iocInit and never saw a driver value.
    ///
    /// Also the DSET seam: the framework reaches the record's scan list through
    /// `AsynRecordDevice` — C's `asynRecordDevice` (`{5,0,0,0,getIoIntInfo,0}`).
    #[test]
    fn an_int32_interrupt_lands_in_i32inp_and_that_cycle_does_no_io() {
        use crate::param::ParamValue;
        use epics_base_rs::server::device_support::DeviceSupport;

        let (rt, reads) = io_intr_port("r11_46_int32");

        let mut rec = AsynRecord::default();
        rec.port = "r11_46_int32".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();

        // iocInit: the DSET hands the framework the record's scan list, which is
        // also C's `interruptAccept` moment.
        let mut dev = AsynRecordDevice::new();
        dev.init(&mut rec).unwrap();
        let mut wakeups = dev.io_intr_receiver().expect("the DSET owns a scan list");

        // SCAN = "I/O Intr" → dbScan `scanAdd` → getIoIntInfo(0) → registerInterrupts.
        rec.set_io_intr_scan(true);
        assert_eq!(rec.errs, "", "the port implements asynInt32");

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );
        wakeups
            .try_recv()
            .expect("C `scanIoRequest` — the callback asks for a process");

        rec.process().unwrap();
        assert_eq!(rec.i32inp, 7, "the pushed value, not a read");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "C `goto done`: the interrupt-driven cycle queues no I/O"
        );

        // The gate is consumed: the next scan is an ordinary read again.
        rec.process().unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1, "gotValue was cleared");
        assert_eq!(rec.i32inp, 42, "…and the driver's read landed");
    }

    /// Negative control for the gate: with the record NOT on the I/O Intr scan
    /// list there is no registration, so a driver value reaches nothing and the
    /// cycle performs its ordinary read. C: `registerInterrupts` runs only from
    /// `getIoIntInfo` (asynRecord.c:591).
    #[test]
    fn a_driver_value_is_ignored_while_the_record_is_not_on_the_io_intr_list() {
        use crate::param::ParamValue;

        let (rt, reads) = io_intr_port("r11_46_not_armed");

        let mut rec = AsynRecord::default();
        rec.port = "r11_46_not_armed".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );

        rec.process().unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "SCAN is Passive: a real read"
        );
        assert_eq!(rec.i32inp, 42, "the driver's read, not the interrupt value");
    }

    /// R11-46. C's callback drops its value when the record has not processed the
    /// previous one: "If gotValue is 1 then the record has not yet processed the
    /// previous interrupt" — `if (pasynRecPvt->gotValue) return` (asynRecord.c:
    /// 717-719,738-740,759-761,780-782). The FIRST unprocessed value is kept, not
    /// the latest.
    #[test]
    fn a_second_interrupt_before_the_record_processes_is_dropped() {
        use crate::param::ParamValue;

        let (rt, _reads) = io_intr_port("r11_46_drop");

        let mut rec = AsynRecord::default();
        rec.port = "r11_46_drop".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );
        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(9),
            crate::interfaces::InterfaceType::Int32,
            0,
        );

        rec.process().unwrap();
        assert_eq!(rec.i32inp, 7, "C keeps the first unprocessed value");

        // The cell reopens once the record consumed it.
        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(11),
            crate::interfaces::InterfaceType::Int32,
            0,
        );
        rec.process().unwrap();
        assert_eq!(rec.i32inp, 11);
    }

    /// R12-46. C dispatches on `state` FIRST: `if (pasynRecPvt->gotValue) goto
    /// done` (asynRecord.c:341) sits *inside* the `state == stateIdle` arm of an
    /// if/else-if chain whose first test is `state` (:331-361). A record with no
    /// port takes the `stateNoDevice` arm — "Not connect to a port" +
    /// STATE_ALARM/MINOR (:356-361) — and never consults the interrupt flag at
    /// all; `done:` then clears it (:370), so the value is DISCARDED, not
    /// published.
    ///
    /// The wave-9 d9xg9 merge placed the gate above the port check, reading
    /// C's :340-341 as textually-above-means-first. This pins the two boundaries
    /// that inversion crossed: the refusal cycle publishes nothing and alarms,
    /// and the sample does not survive it.
    #[test]
    fn an_interrupt_sample_does_not_survive_a_cycle_with_no_port() {
        use crate::param::ParamValue;

        let (rt, _reads) = io_intr_port("r12_46_no_port");

        let mut rec = AsynRecord::default();
        rec.port = "r12_46_no_port".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );

        // The port goes away *after* the driver pushed a value — a PORT put that
        // fails to resolve, which nulls `port_entry` (C `stateNoDevice`).
        rec.port = "R12_46_NO_SUCH_PORT".to_string();
        let _ = rec.connect_device();
        assert!(rec.port_entry.is_none(), "the record now has no port");

        rec.process().unwrap();
        assert_eq!(
            rec.errs, "Not connect to a port",
            "C takes the stateNoDevice arm and never looks at gotValue"
        );
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::STATE_ALARM, AlarmSeverity::Minor),
            "C asynRecord.c:361 alarms the refusal STATE/MINOR"
        );
        assert_eq!(
            rec.i32inp, 0,
            "the stale interrupt value was never published — a portless record \
             must not look healthy and freshly updated"
        );

        // C's `done:` cleared `gotValue` on that refusal cycle (:370), so the
        // value is gone for good: it cannot surface on a later cycle either.
        rec.process().unwrap();
        assert_eq!(
            rec.i32inp, 0,
            "the discarded sample does not resurface on the next cycle"
        );
    }

    /// R12-46, the cell invariant. C clears `gotValue` when it (re)registers
    /// (`getIoIntInfo` cmd == 0, asynRecord.c:589) — "a fresh registration never
    /// inherits a value left over from a previous one" — and again at `done:`
    /// (:370) on every cycle that reaches it. So a value pushed under an old
    /// registration can never be published under a new one.
    #[test]
    fn a_re_armed_scan_does_not_inherit_the_previous_registrations_value() {
        use crate::param::ParamValue;

        let (rt, _reads) = io_intr_port("r12_46_rearm");

        let mut rec = AsynRecord::default();
        rec.port = "r12_46_rearm".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );

        // The operator moves SCAN off "I/O Intr" before the record ever
        // processed that value, then back on: C's cmd==1 cancel followed by a
        // cmd==0 register, which clears the flag.
        rec.set_io_intr_scan(false);
        rec.set_io_intr_scan(true);

        rec.process().unwrap();
        assert_eq!(
            rec.i32inp, 42,
            "the re-armed scan performed a real read — it did not inherit the 7 \
             pushed under the previous registration"
        );
    }

    /// R12-46. The companion boundary: with the port present, the gate still
    /// fires — moving it below the port check must not disable I/O Intr mode.
    #[test]
    fn the_interrupt_gate_still_fires_when_the_record_has_a_port() {
        use crate::param::ParamValue;

        let (rt, reads) = io_intr_port("r12_46_with_port");

        let mut rec = AsynRecord::default();
        rec.port = "r12_46_with_port".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);

        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );

        rec.process().unwrap();
        assert_eq!(rec.i32inp, 7, "the interrupt value, not a read");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "C `goto done` queues no I/O on an interrupt-driven cycle"
        );
        assert_eq!(rec.errs, "", "an interrupt-driven cycle raises nothing");
    }

    /// R11-46. Each IFACE has its own callback writing its own input field:
    /// TINP (escaped, :725), I32INP (:747), UI32INP (:768), F64INP (:789). The
    /// UInt32 registration carries the record's UI32MASK (:635), so only an
    /// overlapping change fires it.
    #[test]
    fn each_iface_interrupt_writes_its_own_input_field() {
        use crate::interfaces::InterfaceType as RegIface;
        use crate::param::ParamValue;

        // Octet → TINP, escaped.
        let (rt, _reads) = io_intr_port("r11_46_octet");
        let mut rec = AsynRecord::default();
        rec.port = "r11_46_octet".to_string();
        rec.iface = InterfaceType::Octet as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);
        fire_interrupt(&rt, 0, ParamValue::Octet("ab\n".into()), RegIface::Octet, 0);
        rec.process().unwrap();
        assert_eq!(rec.tinp, "ab\\n", "C `epicsStrSnPrintEscaped` into TINP");

        // Float64 → F64INP.
        let (rt, _reads) = io_intr_port("r11_46_f64");
        let mut rec = AsynRecord::default();
        rec.port = "r11_46_f64".to_string();
        rec.iface = InterfaceType::Float64 as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);
        fire_interrupt(&rt, 0, ParamValue::Float64(2.5), RegIface::Float64, 0);
        rec.process().unwrap();
        assert_eq!(rec.f64inp, 2.5);

        // UInt32Digital → UI32INP, and the registration carries UI32MASK.
        let (rt, _reads) = io_intr_port("r11_46_ui32");
        let mut rec = AsynRecord::default();
        rec.port = "r11_46_ui32".to_string();
        rec.iface = InterfaceType::UInt32Digital as i32;
        rec.ui32mask = 0x0F;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();
        rec.set_io_intr_scan(true);

        // A change outside UI32MASK never reaches the record (C hands the mask to
        // registerInterruptUser and the driver filters on it).
        fire_interrupt(
            &rt,
            0,
            ParamValue::UInt32Digital(0xF0),
            RegIface::UInt32Digital,
            0xF0,
        );
        rec.process().unwrap();
        assert_eq!(rec.ui32inp, 0, "no bit of the change is in UI32MASK");

        fire_interrupt(
            &rt,
            0,
            ParamValue::UInt32Digital(0x03),
            RegIface::UInt32Digital,
            0x03,
        );
        rec.process().unwrap();
        assert_eq!(rec.ui32inp, 0x03);
    }

    /// R11-46. C `registerInterrupts` refuses the interface the port does not
    /// implement — `reportError(... "No asynInt32 interface")` and `return -1`,
    /// which leaves the record off the scan list (asynRecord.c:625-628).
    #[test]
    fn arming_io_intr_against_a_port_without_the_iface_reports_the_c_text() {
        use crate::interfaces::octet_transport_capabilities;
        use crate::param::ParamValue;
        use crate::port::PortFlags;
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        struct OctetOnly(PortDriverBase);
        impl PortDriver for OctetOnly {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                octet_transport_capabilities()
            }
        }

        let port_name = "r11_46_no_int32";
        let (rt, _jh) = create_port_runtime(
            OctetOnly(PortDriverBase::new(port_name, 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();

        rec.set_io_intr_scan(true);
        assert_eq!(rec.errs, "No asynInt32 interface");

        // …and no registration exists, so a driver value reaches nothing.
        fire_interrupt(
            &rt,
            0,
            ParamValue::Int32(7),
            crate::interfaces::InterfaceType::Int32,
            0,
        );
        rec.process().unwrap();
        assert_eq!(rec.i32inp, 0, "the refused registration pushed nothing");
    }

    /// R11-46. C `cancelIOInterruptScan` (asynRecord.c:794-806): a put that
    /// invalidates what the record subscribed to — REASON (:490), IFACE (:494),
    /// UI32MASK (:497) or a PCNCT=0 detach (:525) — takes the record off the I/O
    /// Intr scan list, which is what cancels the driver registration.
    #[test]
    fn reason_iface_ui32mask_and_pcnct_puts_cancel_the_io_intr_scan() {
        use crate::param::ParamValue;

        let (rt, reads) = io_intr_port("r11_46_cancel");

        let mut rec = AsynRecord::default();
        rec.port = "r11_46_cancel".to_string();
        rec.iface = InterfaceType::Int32 as i32;
        rec.tmod = TransferMode::Read as i32;
        rec.connect_device().unwrap();
        let scan = rec.io_intr_scan();
        let _wakeups = scan.take_receiver().unwrap();

        // The reason the record's registration is bound to; the REASON put moves
        // it, and an interrupt is only delivered for the reason it subscribed to.
        let mut reason = 0usize;

        for field in ["REASON", "IFACE", "UI32MASK", "PCNCT"] {
            rec.set_io_intr_scan(true);
            assert!(scan.is_active(), "{field}: armed");
            // Armed, the registration really does gate a cycle — the control the
            // cancel assertion below is measured against.
            let before = reads.load(Ordering::SeqCst);
            fire_interrupt(
                &rt,
                reason,
                ParamValue::Int32(7),
                crate::interfaces::InterfaceType::Int32,
                0,
            );
            rec.process().unwrap();
            assert_eq!(
                rec.i32inp, 7,
                "{field}: armed, the interrupt drove the cycle"
            );
            assert_eq!(
                reads.load(Ordering::SeqCst),
                before,
                "{field}: and did no I/O"
            );

            match field {
                "REASON" => {
                    rec.reason = 3;
                    reason = 3;
                }
                "IFACE" => rec.iface = InterfaceType::Int32 as i32,
                "UI32MASK" => rec.ui32mask = 0x0F,
                "PCNCT" => rec.pcnct = 0,
                _ => unreachable!(),
            }
            rec.special(field, true).unwrap();
            assert!(
                !scan.is_active(),
                "{field}: C forces SCAN back to Passive, cancelling the registration"
            );

            // PCNCT=0 detached the port (C :522-526); reconnect so the read below
            // has a device. The reconnect must not re-arm the scan by itself.
            if field == "PCNCT" {
                rec.pcnct = 1;
                rec.special("PCNCT", true).unwrap();
                assert!(!scan.is_active(), "a reconnect does not re-arm I/O Intr");
            }

            // The cancelled registration pushes nothing: the record reads instead.
            let before = reads.load(Ordering::SeqCst);
            fire_interrupt(
                &rt,
                reason,
                ParamValue::Int32(7),
                crate::interfaces::InterfaceType::Int32,
                0,
            );
            rec.i32inp = 0;
            rec.process().unwrap();
            assert_eq!(
                reads.load(Ordering::SeqCst),
                before + 1,
                "{field}: no interrupt value gated the cycle"
            );
            assert_eq!(rec.i32inp, 42, "{field}: the value came from the read");
        }
    }
}
