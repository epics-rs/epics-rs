// RTEMS-EXEC-MODEL-ALLOW(17): checked, not waived — all 17 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1572/1572). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
pub mod device;
#[cfg(test)]
pub(crate) mod errs_alloc_tests;
mod io_intr;
pub use device::{ASYN_RECORD_DTYP, AsynRecordDevice};
// The port registry itself is [`crate::registry`], not this module's: it is the
// framework's process-wide name claim, used by `crate::manager` whether or not
// an EPICS record layer exists. Re-exported here because every existing caller
// — `ad-core-rs`, `ad-plugins-rs`, `modbus-rs`, `mqtt-rs` and the example IOCs
// — spells it `asyn_record::get_port`, and moving the definition is not a
// reason to break them.
#[cfg(any(test, feature = "test-support"))]
pub use crate::registry::register_port;
pub use crate::registry::{
    PortClaim, PortEntry, PortRegistry, claim_port, get_port, open_owner, port_names,
    unregister_port,
};

use io_intr::{IoIntrBinding, IoIntrSample, IoIntrScan, RegisterRefusal};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{AsyncDbHandle, ProcessCallback};
use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, CycleHooks, CycleMarks, CyclePostMask, FieldDeclaration,
    FieldMetadataOverride, ProcessOutcome, Record, RecordProcessResult, ScanType,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::drivers::option_parse::{is_option_key, option_key};
use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::{AsynException, ExceptionCallbackGuard};
use crate::interpose::EomReason;
use crate::manager::ErrorMessage;
use crate::port_handle::PortHandle;
use crate::request::{
    CellBody, PortCalls, ReadEntry, RequestCell, RequestOp, RequestResult, Unrun, octet_outcome,
};
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use crate::user::AsynUser;

/// Return the asyn record type factory for injection into IocBuilder.
///
/// This and [`register_asyn_record_type`] are what actually belonged to this
/// module when the port registry was split out to [`crate::registry`]: both
/// name `epics_base_rs` record machinery, so both exist only under the `epics`
/// feature, and neither is reachable from the framework core.
pub fn asyn_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    export_asyn_dir();
    ("asyn", Box::new(|| Box::new(AsynRecord::default())))
}

/// Register the "asyn" record type via the global registry (legacy).
/// Prefer `asyn_record_factory()` with `IocBuilder::register_record_type()`.
pub fn register_asyn_record_type() {
    export_asyn_dir();
    epics_base_rs::server::db_loader::register_record_type(
        "asyn",
        Box::new(|| Box::new(AsynRecord::default())),
    );
}

/// `$(ASYN)` for the startup script, as the IOC's envPaths sets it in C.
fn export_asyn_dir() {
    epics_base_rs::runtime::env::set_default_path("ASYN", crate::ASYN_DIR);
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
    /// `"No asynUInt32Digital interface"` (asynRecord.c:637), spelled in full,
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

/// C `baud_choices` (asynRecord.c:49-52).
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
/// (asynRecord.c:1866-1870), which walks the very array `setOption` writes from.
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

/// Capacity of the `ERRS` diagnostic buffer, in bytes — C `ERR_SIZE`
/// (asynRecord.c:69), the `callocMustSucceed` size at `:301-302` and the
/// `no_elements` both `cvt_dbaddr` (`:958`) and `get_array_info` (`:978`)
/// report. ERRS is not a `DBF_STRING`: `cvt_dbaddr` gives it
/// `field_type = DBF_CHAR` and `dbr_field_type = DBR_CHAR` (`:959-961`), so a
/// CA client sees a 100-element char array, not a 40-byte string. `reportError`
/// formats through `epicsVsnprintf(buffer, ERR_SIZE, ...)` (`:2037`), which
/// leaves at most `ERR_SIZE - 1` characters plus the NUL.
const ERR_SIZE: usize = 100;

/// The `ERRS` text — C's `errs[ERR_SIZE]` (asynRecord.c:207), which
/// `reportError` formats in place (:2037) — at most `ERR_SIZE - 1` bytes.
pub type Errs = crate::fixed_text::FixedText<{ ERR_SIZE - 1 }>;

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
/// asynRecord OEOS/IEOS writes (C asynRecord.c:1965, :1974) and ASCII octet
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
    translate_escape_into(input, &mut out);
    out
}

/// [`translate_escape_bytes`] appending to `out`, whose capacity it reuses.
fn translate_escape_into(input: &[u8], out: &mut Vec<u8>) {
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

// `AsynRecord::own_raise` states.
const RAISE_IDLE: u8 = 0;
const RAISE_OPEN: u8 = 1;
const RAISE_HEARD: u8 = 2;

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
    pub bout: Vec<u8>, // head of the OMAX-byte buffer; reads serve NOWT bytes
    pub omax: i32,
    pub nowt: i32,
    pub nawt: i32,
    pub ofmt: i32, // Output format (menu asynFMT)

    // --- asynOctet input ---
    pub ainp: String,
    pub tinp: String,
    pub ieos: String,
    pub binp: Vec<u8>, // head of the IMAX-byte buffer; reads serve NORD bytes
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
    pub errs: Errs,
    pub aqr: i32,

    // --- Runtime state (not EPICS fields) ---
    device_user: crate::manager::DeviceUser,
    resolved_reason: usize,
    /// C `pasynUser->drvUser`: written only by a successful
    /// `drvUser->create` (asynRecord.c:1250), as C's is.
    drv_user: Option<crate::port::DrvUser>,

    // The record's canonical name plus a cycle-free handle to its own
    // database, handed over by the framework at `add_record` via
    // `set_async_context` (C records reach the IOC the same way at
    // `dbDefineRecord`). Lets `process()` run port I/O off the scan thread
    // and re-enter on completion, and lets the exception callback take the
    // record's hold from the port's side (`with_record`).
    async_ctx: Option<(epics_base_rs::types::RecordName, AsyncDbHandle)>,
    /// The name as the asyn trace prints it, made once here so the cycle
    /// that copies it into its I/O plan never re-validates the bytes.
    trace_name: String,
    /// The callback band this record's `PRIO` selects, refreshed from
    /// `ProcessContext` before every `process()`. C asyn device support
    /// completes through
    /// `callbackRequestProcessCallback(&pPvt->callback, pPvt->pr->prio, pPvt->pr)`
    /// — the record's own PRIO picks the queue the re-entry runs on. Low until
    /// the first cycle, the band an unwritten `PRIO` already has.
    callback_priority: epics_libcom_rs::runtime::task::CallbackPriority,
    /// The `asynCallbackSpecial` requests queued on a blocking port.
    special_lane: Arc<SpecialLane>,
    /// The record's `asynCallbackSpecial` requests — see
    /// [`Self::special_request`].
    special_cells: Vec<SpecialCell>,
    // The process request C makes once with `createAsynUser(asynCallbackProcess,
    // queueTimeoutCallbackProcess)` (asynRecord.c:306-308): the plan the port
    // thread runs and the outcome it leaves for the completion re-entry.
    process_job: Arc<ProcessJob>,
    // That request on the connected port — queued by `process()` without
    // allocating, cancelled by `AQR`.
    process_cell: Option<Arc<RequestCell>>,
    // C `stateIO` (asynRecord.c:216): `process()` queued the request and
    // returned `AsyncPending`; the next entry is its completion.
    io_pending: bool,

    // Set by the exception callback (C `exceptCallback`, asynRecord.c:903-917)
    // when ANY asyn exception fires on the record's port — an external
    // `setTrace{Mask,IOMask,InfoMask}`, a connect / disconnect, an enable or
    // auto-connect change; consumed by `process()`, which re-runs the whole
    // C `monitorStatus` refresh through [`AsynRecord::monitor_status`]. This is
    // the fallback for a record with no database handle or no runtime: with
    // both, the callback refreshes under the record's hold at once, as C
    // does, and never sets this flag.
    status_dirty: Arc<AtomicBool>,
    // The window [`AsynRecord::raise_exception`] holds open while one of the
    // record's own setters runs: `RAISE_OPEN`, or `RAISE_HEARD` once the
    // registered callback has left an exception to the refresh after it.
    own_raise: Arc<AtomicU8>,
    // Owner handle for the registered exception callback so it can
    // be removed on disconnect / drop (C `exceptionCallbackRemove`,
    // asynRecord.c:523,1154,1313).
    except_cb: Option<ExceptionCallbackGuard>,
    // The SCAN put `cancelIOInterruptScan` makes, built once the record has a
    // database, so `special()` leaves it to the put path without allocating.
    cancel_scan: Option<epics_base_rs::server::record::SharedAfterWork>,
    // The put that reached `special()` owes `cancel_scan` once the hold drops.
    cancel_scan_owed: bool,
    // The record alarm `(stat, sevr)` raised by this process cycle's I/O — the C
    // `recGblSetSevr(pasynRec, …)` calls in the `perform*IO` helpers `performIO`
    // dispatches to (asynRecord.c:1364-1636) and the no-asynGpib-interface COMM
    // alarm (:1649/:1695). Set by `apply_io_outcome` / the UCMD-ACMD short-circuits;
    // the single consumer is `check_alarms`, which `take()`s it and commits it
    // via `rec_gbl_set_sevr`. Every complete-returning process cycle reaches
    // `check_alarms`, so the `take()` is the single clear point — no path
    // stages an alarm and then bypasses the consumer.
    io_alarm: Option<(u16, AlarmSeverity)>,
    // C `old.traceFd` (asynRecord.c:203): the identity of the trace sink this
    // record last saw or installed. `monitorStatus` compares the port's current
    // sink against it and writes TFIL = "Unknown" when another thread re-pointed
    // the trace file (:1119-1124). C's `calloc` leaves it NULL, the errlog
    // sink's id, so a first sample of any other sink is a change.
    old_trace_file_id: usize,

    // C `asynRecPvt`'s I/O Intr half — `ioScanPvt`, `interruptPvt`,
    // `interruptLock` and `gotValue` (asynRecord.c:228-232). Shared with the
    // record's `asynRecordDevice` device support, which is the framework's
    // `get_ioint_info` seam; see [`io_intr`].
    io_intr: Arc<IoIntrScan>,

    // C `asynRecPvt.old` (asynRecord.c:147-208), what every `POST_IF_NEW`
    // compares against.
    old: PvtOld,
    // A bit per [`ASYN_POSTS`] entry: the posts C makes under the record's
    // lock, held for the framework to emit under the same hold.
    post_marks: u128,
    // The furthest [`ASYN_POSTS`] entry marked since the last take, so the
    // next mark lands at or after it — see [`AsynRecord::mark`].
    post_cursor: u32,
}

/// C `asynRecPvt.old` (asynRecord.c:147-208): the copy every `POST_IF_NEW`
/// compares against. Zeroed like C's `calloc`ed `asynRecPvt`; only
/// `REMEMBER_STATE` and `POST_IF_NEW` themselves advance it. A field C does
/// not `REMEMBER_STATE` (PCNCT, REASON, DRVINFO, the `*IV` flags, HOSTINFO)
/// posts against the value it last posted, so a put that already stored it
/// still posts from the readback.
#[derive(Default)]
struct PvtOld {
    nrrd: i32,
    nord: i32,
    nowt: i32,
    nawt: i32,
    spr: i32,
    ucmd: i32,
    acmd: i32,
    eomr: i32,
    i32inp: i32,
    ui32inp: u32,
    f64inp: f64,
    /// `monitorStatus`'s fields in [`ASYN_POSTS`] order; TFIL's and DRVINFO's
    /// slots are unused.
    status: [i32; 30],
    drvinfo: OldStr,
    /// `getOptions`'s fields in [`ASYN_POSTS`] order, HOSTINFO's slot unused.
    options: [i32; 12],
    hostinfo: OldStr,
    ieos: OldStr,
    oeos: OldStr,
}

/// A `char[40]` in `asynRecPvt.old`, compared and copied as C's
/// `strncmp`/`strncpy` over the dbd size (asynRecord.dbd `size(40)`), so
/// neither holds the heap.
struct OldStr([u8; 40]);

impl Default for OldStr {
    fn default() -> Self {
        Self([0; 40])
    }
}

impl OldStr {
    /// C `REMEMBER_STATE` for a string.
    fn remember(&mut self, cur: &str) {
        self.0 = Self::of(cur);
    }

    /// C `if (strncmp(cur, old, 40)) { strncpy(old, cur, 40); post; }`.
    fn post_if_new(&mut self, cur: &str) -> bool {
        let cur = Self::of(cur);
        std::mem::replace(&mut self.0, cur) != cur
    }

    fn of(cur: &str) -> [u8; 40] {
        let mut buf = [0; 40];
        let n = cur.len().min(40);
        buf[..n].copy_from_slice(&cur.as_bytes()[..n]);
        buf
    }
}

/// Every `db_post_events` C asynRecord makes under the record's lock, in the
/// order one hold makes them. Each is a mark in `post_marks`, never a
/// deferred post: `post_fields` stores what it publishes, so a deferral that
/// carries its value writes it back when it lands, and taking a lock or
/// spawning under the hold is barred.
///
/// The first [`HELD_POSTS`] entries are the posts `process()` and `special()`
/// make themselves, drained as each returns, all `DBE_VALUE | DBE_LOG`:
/// `reportError`'s ERRS (:2047), `monitorStatus` (:1102-1139), `getOptions`
/// (:1928-1943), `getEos` (:2016-2024), ERRS again, `monitorStatus` again. Two
/// copies of a post cover both of C's orders: `connectDevice` runs
/// `monitorStatus` before its readbacks and again after (:1271, :1319), while
/// `asynCallbackSpecial` reads back and then runs it (:845-897).
///
/// The rest are C `monitor()`'s, in its order (asynRecord.c:1006-1031), each
/// with `recGblResetAlarms | DBE_VALUE | DBE_LOG`: the input buffer and TINP
/// on a reading TMOD, then the eleven `POST_IF_NEW`s.
const ASYN_POSTS_NAMED: [(&str, CyclePostMask); 90] = [
    ("ERRS", CyclePostMask::ValueLog),
    ("TMSK", CyclePostMask::ValueLog),
    ("TB0", CyclePostMask::ValueLog),
    ("TB1", CyclePostMask::ValueLog),
    ("TB2", CyclePostMask::ValueLog),
    ("TB3", CyclePostMask::ValueLog),
    ("TB4", CyclePostMask::ValueLog),
    ("TB5", CyclePostMask::ValueLog),
    ("TIOM", CyclePostMask::ValueLog),
    ("TIB0", CyclePostMask::ValueLog),
    ("TIB1", CyclePostMask::ValueLog),
    ("TIB2", CyclePostMask::ValueLog),
    ("TINM", CyclePostMask::ValueLog),
    ("TINB0", CyclePostMask::ValueLog),
    ("TINB1", CyclePostMask::ValueLog),
    ("TINB2", CyclePostMask::ValueLog),
    ("TINB3", CyclePostMask::ValueLog),
    ("TSIZ", CyclePostMask::ValueLog),
    ("TFIL", CyclePostMask::ValueLog),
    ("AUCT", CyclePostMask::ValueLog),
    ("CNCT", CyclePostMask::ValueLog),
    ("PCNCT", CyclePostMask::ValueLog),
    ("REASON", CyclePostMask::ValueLog),
    ("DRVINFO", CyclePostMask::ValueLog),
    ("ENBL", CyclePostMask::ValueLog),
    ("OCTETIV", CyclePostMask::ValueLog),
    ("OPTIONIV", CyclePostMask::ValueLog),
    ("GPIBIV", CyclePostMask::ValueLog),
    ("I32IV", CyclePostMask::ValueLog),
    ("UI32IV", CyclePostMask::ValueLog),
    ("F64IV", CyclePostMask::ValueLog),
    ("BAUD", CyclePostMask::ValueLog),
    ("LBAUD", CyclePostMask::ValueLog),
    ("PRTY", CyclePostMask::ValueLog),
    ("SBIT", CyclePostMask::ValueLog),
    ("DBIT", CyclePostMask::ValueLog),
    ("MCTL", CyclePostMask::ValueLog),
    ("FCTL", CyclePostMask::ValueLog),
    ("IXON", CyclePostMask::ValueLog),
    ("IXOFF", CyclePostMask::ValueLog),
    ("IXANY", CyclePostMask::ValueLog),
    ("DRTO", CyclePostMask::ValueLog),
    ("HOSTINFO", CyclePostMask::ValueLog),
    ("IEOS", CyclePostMask::ValueLog),
    ("OEOS", CyclePostMask::ValueLog),
    ("ERRS", CyclePostMask::ValueLog),
    ("TMSK", CyclePostMask::ValueLog),
    ("TB0", CyclePostMask::ValueLog),
    ("TB1", CyclePostMask::ValueLog),
    ("TB2", CyclePostMask::ValueLog),
    ("TB3", CyclePostMask::ValueLog),
    ("TB4", CyclePostMask::ValueLog),
    ("TB5", CyclePostMask::ValueLog),
    ("TIOM", CyclePostMask::ValueLog),
    ("TIB0", CyclePostMask::ValueLog),
    ("TIB1", CyclePostMask::ValueLog),
    ("TIB2", CyclePostMask::ValueLog),
    ("TINM", CyclePostMask::ValueLog),
    ("TINB0", CyclePostMask::ValueLog),
    ("TINB1", CyclePostMask::ValueLog),
    ("TINB2", CyclePostMask::ValueLog),
    ("TINB3", CyclePostMask::ValueLog),
    ("TSIZ", CyclePostMask::ValueLog),
    ("TFIL", CyclePostMask::ValueLog),
    ("AUCT", CyclePostMask::ValueLog),
    ("CNCT", CyclePostMask::ValueLog),
    ("PCNCT", CyclePostMask::ValueLog),
    ("REASON", CyclePostMask::ValueLog),
    ("DRVINFO", CyclePostMask::ValueLog),
    ("ENBL", CyclePostMask::ValueLog),
    ("OCTETIV", CyclePostMask::ValueLog),
    ("OPTIONIV", CyclePostMask::ValueLog),
    ("GPIBIV", CyclePostMask::ValueLog),
    ("I32IV", CyclePostMask::ValueLog),
    ("UI32IV", CyclePostMask::ValueLog),
    ("F64IV", CyclePostMask::ValueLog),
    ("AINP", CyclePostMask::MonitorValueLog),
    ("BINP", CyclePostMask::MonitorValueLog),
    ("TINP", CyclePostMask::MonitorValueLog),
    ("NRRD", CyclePostMask::MonitorValueLog),
    ("NORD", CyclePostMask::MonitorValueLog),
    ("NOWT", CyclePostMask::MonitorValueLog),
    ("NAWT", CyclePostMask::MonitorValueLog),
    ("SPR", CyclePostMask::MonitorValueLog),
    ("UCMD", CyclePostMask::MonitorValueLog),
    ("ACMD", CyclePostMask::MonitorValueLog),
    ("EOMR", CyclePostMask::MonitorValueLog),
    ("I32INP", CyclePostMask::MonitorValueLog),
    ("UI32INP", CyclePostMask::MonitorValueLog),
    ("F64INP", CyclePostMask::MonitorValueLog),
];
static ASYN_POSTS: [(epics_base_rs::server::record::FieldIx, CyclePostMask); 90] =
    epics_base_rs::server::record::declared_post_table(
        epics_base_rs::server::record::dbd_generated::ASYN_FIELDS,
        ASYN_POSTS_NAMED,
    );

/// [`ASYN_POSTS`] entries ahead of `monitor()`'s.
const HELD_POSTS: u32 = 76;
const HELD_MARKS: u128 = (1 << HELD_POSTS) - 1;
/// Where each post sits in [`ASYN_POSTS`]: the second copy of a post is the
/// one a mark takes once the first is behind the cursor.
const ERRS_AT: [u32; 2] = [0, 45];
const STATUS_AT: [u32; 2] = [1, 46];
const OPTIONS_AT: u32 = 31;
const EOS_AT: u32 = 43;

const MONITOR_CLAIMED: [&str; 14] = [
    "AINP", "BINP", "TINP", "NRRD", "NORD", "NOWT", "NAWT", "SPR", "UCMD", "ACMD", "EOMR",
    "I32INP", "UI32INP", "F64INP",
];

/// C `POST_IF_NEW` (asynRecord.c:210-214): whether `cur` moved off `old`, with
/// `old` advanced to it.
fn post_if_new<T: PartialEq + Copy>(cur: T, old: &mut T) -> bool {
    let moved = cur != *old;
    *old = cur;
    moved
}

// BOUT/BINP hold the written head of C's `cap`-byte `optr`/`iptr`, which
// `init_record` callocs (asynRecord.c:295-298): bytes past the head are zero.

/// Write `data` over the buffer's head, clamped to `cap` as `dbPut` clamps
/// `nRequest` (dbAccess.c:1365-1366); the bytes past it keep their value.
fn write_head(buf: &mut Vec<u8>, data: &[u8], cap: i32) -> i32 {
    let n = data.len().min(cap.max(0) as usize);
    if buf.len() < n {
        buf.resize(n, 0);
    }
    buf[..n].copy_from_slice(&data[..n]);
    n as i32
}

/// Append the buffer's first `count` bytes, as `get_array_info` reports them
/// (asynRecord.c:966-976). C does not clamp `count` to the buffer, so a client
/// NOWT past OMAX reads past `optr`; clamped here.
fn extend_head(out: &mut Vec<u8>, buf: &[u8], count: i32, cap: i32) {
    let n = count.clamp(0, cap.max(0)) as usize;
    out.extend_from_slice(&buf[..n.min(buf.len())]);
    out.resize(out.len() + n.saturating_sub(buf.len()), 0);
}

fn read_head(buf: &[u8], count: i32, cap: i32) -> Vec<u8> {
    let mut out = Vec::new();
    extend_head(&mut out, buf, count, cap);
    out
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
            errs: Errs::EMPTY,
            aqr: 0,
            device_user: crate::manager::DeviceUser::default(),
            resolved_reason: 0,
            drv_user: None,
            async_ctx: None,
            trace_name: String::new(),
            callback_priority: epics_libcom_rs::runtime::task::CallbackPriority::Low,
            special_lane: Arc::default(),
            special_cells: Vec::new(),
            process_job: Arc::default(),
            process_cell: None,
            io_pending: false,
            status_dirty: Arc::new(AtomicBool::new(false)),
            except_cb: None,
            own_raise: Arc::default(),
            cancel_scan: None,
            cancel_scan_owed: false,
            io_alarm: None,
            old_trace_file_id: TraceFile::Errlog.id(),
            io_intr: Arc::new(IoIntrScan::new()),
            old: PvtOld::default(),
            post_marks: 0,
            post_cursor: 0,
        }
    }
}

// ===== Blocking-port I/O: the process request =====
//
// C asynRecord queues `performIO` as one request (`queueRequest`,
// asynRecord.c:342) that the port thread runs in `asynCallbackProcess`
// (asynRecord.c:808-832); `process()` returns with `pact=TRUE` and the
// record completes on the callback's `callbackRequestProcessCallback`
// re-process. Here the request is the record's [`RequestCell`], its callbacks
// are [`ProcessJob`]'s, and the re-process re-enters `process()` through the
// PACT async-record primitive.

/// The record's process request — what C's `asynCallbackProcess` and
/// `queueTimeoutCallbackProcess` read and write (asynRecord.c:808-832,
/// :919-927). The record writes the plan in place before it queues; the port
/// thread leaves the outcome for the completion re-entry.
#[derive(Default)]
struct ProcessJob {
    request: parking_lot::Mutex<ProcessRequest>,
    outcome: parking_lot::Mutex<Option<IoOutcome>>,
    /// The completion's `epicsCallback`, kept in the record's private structure
    /// as C keeps it in `asynRecPvt` (asynRecord.c:227); set at `add_record`.
    reentry: parking_lot::Mutex<Option<ProcessCallback>>,
}

struct ProcessRequest {
    plan: IoPlan,
    /// C `callbackRequestProcessCallback(..., pasynRec->prio, ...)`.
    prio: epics_libcom_rs::runtime::task::CallbackPriority,
    /// C `asynCallbackProcess`'s `canBlock` test (asynRecord.c:828-831): only
    /// a queued cycle ends in the re-process; one `queueRequest` ran in place
    /// is applied by its caller.
    queued: bool,
}

impl Default for ProcessRequest {
    fn default() -> Self {
        Self {
            plan: IoPlan::default(),
            prio: epics_libcom_rs::runtime::task::CallbackPriority::Low,
            queued: false,
        }
    }
}

impl CellBody for ProcessJob {
    /// C `asynCallbackProcess`'s `performIO` (asynRecord.c:819-827).
    fn process(&self, port: &mut dyn PortCalls) {
        let out = run_io_phases(&mut self.request.lock().plan, port);
        *self.outcome.lock() = Some(out);
    }

    /// C `queueTimeoutCallbackProcess` (asynRecord.c:919-927), the `AQR`
    /// `wasQueued` branch (:397-404), or a refusal at dequeue.
    fn unrun(&self, why: Unrun) {
        let mut out = IoOutcome::default();
        {
            let trace = &self.request.lock().plan.trace;
            match why {
                Unrun::Canceled => out.report_canceled(trace),
                Unrun::TimedOut => out.report_queue_timeout(trace),
                Unrun::Refused(_) => out.report_queue_refused(trace),
            }
        }
        *self.outcome.lock() = Some(out);
    }

    /// C `callbackRequestProcessCallback`. The re-entry takes the record's
    /// hold, which a record waiting on this port may hold, so it never runs on
    /// the port's thread.
    fn done(self: Arc<Self>) {
        let (prio, queued) = {
            let request = self.request.lock();
            (request.prio, request.queued)
        };
        if !queued {
            return;
        }
        if let Some(cb) = self.reentry.lock().as_ref() {
            // asynRecord ignores the status (asynRecord.c:830-831).
            let _ = cb.request(prio);
        }
    }
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
    /// (asynRecord.c:1698-1749).
    Addressed(Vec<u8>),
    /// C's three-operation Serial Poll (asynRecord.c:1717-1746).
    SerialPoll,
}

/// C `reportError`'s trace line (asynRecord.c:2037-2040): `asynPrint(pasynUser,
/// ASYN_TRACE_ERROR, "%s: %s\n", pasynRec->name, buffer)` unless the status is
/// success or timeout.
fn trace_report(
    trace: &TraceManager,
    port: Option<&str>,
    addr: Option<i32>,
    reason: i32,
    name: &str,
    status: AsynStatus,
    errs: &str,
) {
    if matches!(status, AsynStatus::Success | AsynStatus::Timeout) {
        return;
    }
    trace.print(
        port,
        addr,
        reason,
        TraceMask::ERROR,
        format_args!("{name}: {errs}\n"),
    );
}

/// What a report made on the port thread prints with — the record's
/// `pasynUser` rung and `pasynRec->name` — copied into the plan under the hold
/// into buffers the plan keeps.
#[derive(Default)]
struct IoTrace {
    trace: Option<Arc<TraceManager>>,
    /// `None`: a user on no port, which prints on the global rung.
    port: Option<String>,
    addr: Option<i32>,
    reason: i32,
    name: String,
}

impl IoTrace {
    fn report(&self, status: AsynStatus, errs: &str) {
        if let Some(trace) = &self.trace {
            let port = self.port.as_deref();
            trace_report(
                trace,
                port,
                self.addr,
                self.reason,
                &self.name,
                status,
                errs,
            );
        }
    }
}

/// Snapshot of the record fields `performIO` reads, written on the scan thread
/// so the port thread never touches the record.
struct IoPlan {
    trace: IoTrace,
    tmod: TransferMode,
    iface: InterfaceType,
    /// `Some` when this cycle is a GPIB command rather than a transfer — the
    /// UCMD / ACMD the operator put, decoded by [`AsynRecord::gpib_cycle`].
    gpib: Option<GpibCycle>,
    /// The interface this cycle dispatches on and the port lacks: C
    /// `performIO`'s `!octetiv`.. arms and `gpibUniversalCmd` /
    /// `gpibAddressedCmd`'s `!gpibiv` (asynRecord.c:1328-1360, :1647, :1693),
    /// which refuse inside the queued callback.
    missing: Option<&'static str>,
    /// The NOWT and NRRD `performOctetIO` clamps into the record
    /// (asynRecord.c:1499, :1514), `Some` only where it changes them.
    nowt_clamp: Option<i32>,
    nrrd_clamp: Option<i32>,
    // Per-request `asynUser` inputs. Stored as primitives (not a built
    // `AsynUser`) because `AsynUser` owns a non-`Clone` `user_data` box, and
    // every phase consumes a fresh user by value (`io_user`/`flush_user`).
    reason: usize,
    addr: i32,
    drv_user: Option<crate::port::DrvUser>,
    /// `None` is C's negative TMOT — wait forever; see
    /// [`crate::user::AsynUser::timeout`].
    timeout: Option<std::time::Duration>,
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
    // length and the overflow threshold (`:1602-1621`); `octet_buf_size` is the
    // per-request read length (`min(NRRD, inlen)` or `inlen`), so an
    // NRRD-limited short read can never reach `in_len` and never reads as
    // overflow — the same reason C's check is independent of NRRD.
    in_len: usize,
    ifmt: i32,
    /// The record user's `pasynUser->errorMessage` as the cycle starts.
    error_message: ErrorMessage,
}

impl Default for IoPlan {
    fn default() -> Self {
        Self {
            trace: IoTrace::default(),
            tmod: TransferMode::NoIo,
            iface: InterfaceType::Octet,
            gpib: None,
            missing: None,
            nowt_clamp: None,
            nrrd_clamp: None,
            reason: 0,
            addr: 0,
            drv_user: None,
            timeout: None,
            octet_out: Vec::new(),
            octet_out_len: 0,
            ofmt: 0,
            i32out: 0,
            ui32out: 0,
            ui32mask: 0,
            f64out: 0.0,
            octet_buf_size: 0,
            in_len: 0,
            ifmt: 0,
            error_message: ErrorMessage::EMPTY,
        }
    }
}

/// The record fields `performIO` writes from the I/O results. Each `Some`
/// is applied to the record on the completion re-entry; `None` leaves the
/// field untouched (the phase did not run / produced no value).
#[derive(Default)]
struct IoOutcome {
    /// What C's callback writes into the record before any transfer: the
    /// NOWT / NRRD clamps and the UCMD / ACMD reset (asynRecord.c:821-826,
    /// :1499, :1514). Set by [`IoOutcome::entered`] only, so a cycle that never
    /// ran — refused, timed out, cancelled — leaves those fields as they were.
    nowt: Option<i32>,
    nrrd: Option<i32>,
    ucmd: Option<i32>,
    acmd: Option<i32>,
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
    errs: Option<Errs>,
    /// Record alarm `(stat, sevr)` this I/O cycle raises — the C
    /// `recGblSetSevr(pasynRec, …)` calls scattered through the `perform*IO`
    /// helpers `performIO` dispatches to (asynRecord.c:1364-1636). Accumulated
    /// with the "raise only if strictly higher" rule via [`raise_io_alarm`] so a
    /// write-then-read `Write_Read` keeps the first equal-severity stat, exactly as
    /// C's repeated `recGblSetSevr` does. Applied to the record in
    /// [`AsynRecord::apply_io_outcome`] and committed by [`AsynRecord::check_alarms`].
    alarm: Option<(u16, AlarmSeverity)>,
    /// The `pasynUser->errorMessage` the cycle's last failing call left, which
    /// outlives the cycle on the record's user.
    error_message: Option<ErrorMessage>,
}

impl IoOutcome {
    /// The outcome of a cycle C's `asynCallbackProcess` entered: the field
    /// writes it makes whatever the transfers then do.
    fn entered(plan: &IoPlan) -> Self {
        let gpib = |universal: bool| match plan.gpib {
            Some(GpibCycle::Universal(_)) => universal.then_some(0),
            Some(_) => (!universal).then_some(0),
            None => None,
        };
        Self {
            nowt: plan.nowt_clamp,
            nrrd: plan.nrrd_clamp,
            ucmd: gpib(true),
            acmd: gpib(false),
            ..Self::default()
        }
    }

    fn note_error(&mut self, message: impl std::fmt::Display) {
        self.error_message = Some(ErrorMessage::format(format_args!("{message}")));
    }

    /// `pasynUser->errorMessage` as it stands: this cycle's last failure, or
    /// what an earlier one left. Nothing clears it.
    fn error_message(&self, plan: &IoPlan) -> ErrorMessage {
        self.error_message.unwrap_or(plan.error_message)
    }

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
    ///
    /// C's `reportError` prints there and then, on the port thread, unless the
    /// status is success or timeout — every report, so a cycle whose write and
    /// read both fail prints two lines and ERRS keeps the second.
    fn report_error(&mut self, trace: &IoTrace, status: AsynStatus, msg: std::fmt::Arguments<'_>) {
        let errs = Errs::format(msg);
        trace.report(status, &errs);
        self.errs = Some(errs);
    }

    /// The AQR-cancel outcome, C `special()`'s `wasQueued` branch
    /// (asynRecord.c:397-404): `reportError(… "I/O request canceled")` **and**
    /// `recGblSetSevr(pasynRec, STATE_ALARM, MAJOR_ALARM)`, then the forced
    /// completion callback. The message and the severity are one event in C and
    /// have one owner here, so a cancel cannot land in `ERRS` with the record
    /// left in NO_ALARM. The completion re-entry that applies this outcome
    /// (`apply_io_outcome` → `check_alarms`) is C's forced callback.
    fn report_canceled(&mut self, trace: &IoTrace) {
        // `cancelRequest`'s status, which is success when it found the request.
        self.report_error(trace, AsynStatus::Success, format_args!("{CANCELED_MSG}"));
        raise_io_alarm(self, alarm_status::STATE_ALARM, AlarmSeverity::Major);
    }

    /// The queue-timeout outcome of a **process** request, C
    /// `queueTimeoutCallbackProcess` (asynRecord.c:919-927): `reportError(…
    /// "process queueRequest timeout")`, `recGblSetSevr(STATE_ALARM,
    /// MAJOR_ALARM)`, then `callbackRequestProcessCallback` to complete the
    /// record that is still sitting at `pact = TRUE`.
    ///
    /// The same shape as [`Self::report_canceled`], and for the same reason: both
    /// are "the queued request left the queue without running", so the message
    /// and the severity are one event with one owner, and the completion re-entry
    /// that applies this outcome is C's forced callback. The remaining phases of
    /// the plan do **not** run — C never entered `performIO` at all.
    fn report_queue_timeout(&mut self, trace: &IoTrace) {
        self.report_error(
            trace,
            AsynStatus::Error,
            format_args!("{PROCESS_QUEUE_TIMEOUT_MSG}"),
        );
        raise_io_alarm(self, alarm_status::STATE_ALARM, AlarmSeverity::Major);
    }

    /// The **refused** process request, C `process()` (asynRecord.c:342-361): the
    /// queue gate turned `queueRequest` down (`asynDisabled` / `asynDisconnected`,
    /// asynManager.c:1543-1554), so `performIO` never ran. C reports the literal
    /// `"queueRequest failed"` and alarms `STATE_ALARM`/`MINOR_ALARM` — the same
    /// arm that answers a record with no port ("Not connect to a port", :356-357)
    /// — and posts no transfer field, because there was no transfer.
    ///
    /// The process-side twin of [`AsynRecord::report_never_ran`]: a
    /// refusal is `queueRequest`'s *return value*, not a driver diagnostic raised
    /// inside a callback that ran, so it must not be dressed up as one (R14-46).
    /// Without this the port reported a disabled port as `"Read error, port X is
    /// disabled"` with a COMM/MAJOR alarm and published NORD/EOMR for a read that
    /// never reached the driver.
    fn report_queue_refused(&mut self, trace: &IoTrace) {
        // C passes `asynSuccess` here (asynRecord.c:355): no trace line.
        self.report_error(
            trace,
            AsynStatus::Success,
            format_args!("{PROCESS_QUEUE_REFUSED_MSG}"),
        );
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
/// (recGbl.c:258-261, [`rec_gbl_set_sevr`]): a new severity replaces the pending
/// alarm only when it is **strictly higher** — `if (prec->nsev < new_sevr)` in
/// the `recGblSetSevrVMsg` the wrapper delegates to (recGbl.c:242) — so an
/// equal-severity later call keeps the earlier `stat`. C's `performIO` relies on this when both the
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
/// before every transfer. Built from the plan, so the port thread never reads
/// the record.
fn io_user(plan: &IoPlan) -> AsynUser {
    AsynUser::new(plan.reason)
        .with_addr(plan.addr)
        .with_timeout_opt(plan.timeout)
        .with_queue_timeout(QUEUE_TIMEOUT)
        .with_drv_user(plan.drv_user.clone())
}

/// Build the `asynUser` for a `Flush` phase. C runs the flush on the record's
/// one queued `pasynUser`, so it carries the same TMOT as the transfers
/// (`pasynUser->timeout = pasynRec->tmot`, asynRecord.c:818; the flush call at
/// :1521 does not touch it). The port leaves it unset because nothing reads it:
/// no driver `flush` consults `user.timeout`, and `InterposeFlush` overwrites
/// it with its own configured value and restores it (C does the same,
/// asynInterposeFlush.c:117,122,131) into a per-phase user no later phase sees.
fn flush_user(plan: &IoPlan) -> AsynUser {
    AsynUser::new(plan.reason)
        .with_addr(plan.addr)
        .with_queue_timeout(QUEUE_TIMEOUT)
        .with_drv_user(plan.drv_user.clone())
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
    /// C `gpibAddressedCmd`'s `addressedCmd(acmd, lenCmd)` (asynRecord.c:1748-1749);
    /// the frame is [`GpibCycle::Addressed`]'s.
    GpibAddressed,
    /// Serial Poll step 1 — `universalCmd(IBSPE)` (asynRecord.c:1720-1726).
    GpibSerialPollEnable,
    /// Serial Poll step 2 — a one-byte octet read into SPR
    /// (asynRecord.c:1729-1735).
    GpibSerialPollRead,
    /// Serial Poll step 3 — `universalCmd(IBSPD)` (asynRecord.c:1738-1744).
    /// C runs it even when the enable or the read failed, so it is a phase like
    /// any other: a failing phase reports, it does not abort the cycle.
    GpibSerialPollDisable,
    /// `TMOD = NoI/O`: C still queues the cycle (asynRecord.c:342-353) and the
    /// callback simply calls nothing (`:827`). The phase carries the queue
    /// entry — the port-thread wake and the queue gate — and no transfer.
    NoIo,
    /// The callback's refusal of a cycle on an interface the port lacks
    /// ([`IoPlan::missing`]): a queue entry like [`Self::NoIo`] whose report
    /// is "No %s interface".
    NoInterface(&'static str),
}

/// The phases one `performIO` cycle runs, in C's execution order — the single
/// owner of "which phases, and when" for [`run_io_phases`].
///
/// C `performOctetIO` (asynRecord.c:1518-1558):
///
/// 1. **Flush, before the write**, for `TMOD == Flush` *and* `TMOD ==
///    Write_Read` (`:1518-1523`). Write/Read is the default TMOD, and this is
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
fn io_phases(plan: &IoPlan) -> PhaseList {
    if let Some(name) = plan.missing {
        return PhaseList::of(&[IoPhase::NoInterface(name)]);
    }
    // A GPIB command replaces the transfer entirely — C dispatches
    // gpibUniversalCmd / gpibAddressedCmd *instead of* performIO
    // (asynRecord.c:819-827), so TMOD and IFACE do not apply to this cycle.
    if let Some(cycle) = &plan.gpib {
        return match cycle {
            GpibCycle::Universal(cmd) => PhaseList::of(&[IoPhase::GpibUniversal(*cmd)]),
            GpibCycle::Addressed(_) => PhaseList::of(&[IoPhase::GpibAddressed]),
            GpibCycle::SerialPoll => PhaseList::of(&[
                IoPhase::GpibSerialPollEnable,
                IoPhase::GpibSerialPollRead,
                IoPhase::GpibSerialPollDisable,
            ]),
        };
    }
    // C's `queueRequest` does not consult TMOD: a NoI/O cycle occupies the queue
    // and wakes the port thread exactly like a transfer, and only the callback
    // body is empty (asynRecord.c:342-353, :827). The `stateIdle` arm's one skip
    // is `if(pasynRecPvt->gotValue) goto done` (`:341`), the I/O Intr readback —
    // never a transfer mode. Modelling it as "no phases" made
    // the cycle touch the port at all — so the reconnect-nudge idiom poked
    // nothing and a down port raised no alarm (R18-87).
    if plan.tmod == TransferMode::NoIo {
        return PhaseList::of(&[IoPhase::NoIo]);
    }
    let mut phases = PhaseList::of(&[]);
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

/// At most three phases, on the stack — a cycle queued from under the hold
/// builds its phase list without allocating.
struct PhaseList {
    phases: [IoPhase; 3],
    len: usize,
}

impl PhaseList {
    fn of(phases: &[IoPhase]) -> Self {
        let mut list = Self {
            phases: [IoPhase::NoIo; 3],
            len: 0,
        };
        for &phase in phases {
            list.push(phase);
        }
        list
    }

    fn push(&mut self, phase: IoPhase) {
        self.phases[self.len] = phase;
        self.len += 1;
    }

    fn iter(&self) -> impl Iterator<Item = IoPhase> + '_ {
        self.phases[..self.len].iter().copied()
    }
}

/// Build the write-phase `RequestOp` for an interface. C `performOctetIO`
/// (asynRecord.c:1528-1546) suppresses the driver's output EOS for a binary
/// write; `OctetWriteBinary` brackets that save/clear/restore in the actor.
fn io_write_op(plan: &mut IoPlan) -> RequestOp {
    match plan.iface {
        InterfaceType::Octet => {
            let data = std::mem::take(&mut plan.octet_out);
            if plan.ofmt == ASYN_FMT_BINARY {
                RequestOp::OctetWriteBinary { data }
            } else {
                RequestOp::OctetWrite { data }
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
                    entry: ReadEntry::new(),
                }
            } else {
                RequestOp::OctetRead {
                    buf_size: plan.octet_buf_size,
                    entry: ReadEntry::new(),
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
/// for [`run_io_phases`].
/// A phase's op. Its bytes are lent from the plan, as C writes from the
/// record's own buffer; [`return_phase_buffer`] hands them back.
fn io_phase_op(plan: &mut IoPlan, phase: IoPhase) -> RequestOp {
    match phase {
        IoPhase::Flush => RequestOp::Flush,
        IoPhase::Write => io_write_op(plan),
        IoPhase::Read => io_read_op(plan),
        IoPhase::GpibUniversal(cmd) => RequestOp::GpibUniversalCmd { cmd },
        IoPhase::GpibAddressed => RequestOp::GpibAddressedCmd {
            data: match &mut plan.gpib {
                Some(GpibCycle::Addressed(frame)) => std::mem::take(frame),
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
        IoPhase::GpibSerialPollRead => RequestOp::OctetRead {
            buf_size: 1,
            entry: ReadEntry::new(),
        },
        IoPhase::GpibSerialPollDisable => RequestOp::GpibUniversalCmd {
            cmd: crate::interfaces::gpib::IBSPD,
        },
        IoPhase::NoIo | IoPhase::NoInterface(_) => RequestOp::NoIo,
    }
}

/// The `asynUser` one phase submits with. C gives every transfer the record's
/// TMOT (`pasynUser->timeout = pasynRec->tmot`, asynRecord.c:818) — including
/// the GPIB commands and the flush, which run under that same queued user. The
/// port carries it on every phase but the flush; see [`flush_user`].
fn io_phase_user(plan: &IoPlan, phase: IoPhase) -> AsynUser {
    match phase {
        IoPhase::Flush => flush_user(plan),
        _ => io_user(plan),
    }
}

/// Record a write-phase result into the outcome — the C `performIO` write
/// branch (asynRecord.c:1524-1556 octet, :1370-1382, :1405-1418, :1442-1454
/// register). A write failure reports `ERRS` but, like C, does not skip
/// the read phase.
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
            out.report_error(
                &plan.trace,
                e.status(),
                format_args!(
                    "{} write error, {}",
                    plan.iface.c_errs_name(),
                    e.message_display()
                ),
            );
            raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
        }
        return;
    }

    // C performOctetIO write branch (asynRecord.c:1524-1556). `nbytesTransferred`
    // is what the *device* took: C seeds it to 0 (:1527), the octet chain writes
    // it out on success and on failure alike, and the record commits it —
    // `nawt = nbytesTransferred` (:1547) runs *before* the status test at :1551.
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
        // C's ERRS never carries.) A short write that reported success
        // splices the stale `errorMessage`.
        let tail = out.error_message(plan);
        let status = err.as_ref().map_or(AsynStatus::Success, AsynError::status);
        out.report_error(
            &plan.trace,
            status,
            format_args!("Write error, nout={nawt}, {}", tail.as_str()),
        );
    }
}

/// Record a read-phase result into the outcome — the C `performIO` read
/// branch (asynRecord.c:1557-1635 octet, :1383-1395, :1419-1433,
/// :1455-1467 register).
fn record_read_result(plan: &IoPlan, out: &mut IoOutcome, res: AsynResult<RequestResult>) {
    match plan.iface {
        InterfaceType::Octet => {
            // C performOctetIO reads into a buffer it memset to zero
            // (asynRecord.c:1560) and then assigns EOMR / the IFMT-selected
            // input field / NORD / TINP from `nbytesTransferred` **regardless
            // of the returned status** (:1591-1629) — the error branch at
            // :1583 reports the error and raises the alarm but does not skip
            // the assignments. So a device that emits a partial line and then
            // goes quiet lands AINP="abc", NORD=3 *and* a READ_ALARM.
            //
            // A read that never ran reports a zero transfer, which is exactly
            // C's memset buffer. Register reads keep their stale-on-error
            // fields, matching their C handlers which never memset.
            let read = octet_outcome(res);
            let eom = read.eom().bits();
            let (data, status) = read.into_parts();
            let err = status.err();

            // C `nbytesTransferred` — the count every read-side diagnostic and
            // field assignment below is built from. Taken before `data` is
            // moved into the IFMT-selected input field.
            let nread = data.len();

            out.eomr = Some(eom as i32);
            // NORD is the raw transfer count (C `nord = nbytesTransferred`,
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
                // C: reportError(status, "Error %s", pasynUser->errorMessage)
                // (:1583-1584), which this cycle's next report overwrites in
                // ERRS but whose trace line is printed.
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!("Error {}", e.message_display()),
                );
                // C: reportError(status, "%s  nread %d %s", <status word>,
                // nbytesTransferred, pasynUser->errorMessage) — two spaces after
                // the status word, verbatim from the C format string.
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "{}  nread {} {}",
                        c_read_status_word(e),
                        nread,
                        e.message_display()
                    ),
                );
                // C performOctetIO read error -> recGblSetSevr(READ_ALARM,
                // MAJOR) (asynRecord.c:1599).
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
            if overflow {
                // C: reportError(status, "Overflow nread %d %s",
                // nbytesTransferred, pasynUser->errorMessage) in all three
                // overflow branches (asynRecord.c:1602-1621), alongside the
                // MINOR alarm. The `%s` tail is the driver's message; an
                // overflow that did not *fail* has none, and C then splices
                // whatever is left in its long-lived `pasynUser`.
                let tail = out.error_message(plan);
                // C's `status` is the read's.
                let status = err.as_ref().map_or(AsynStatus::Success, AsynError::status);
                out.report_error(
                    &plan.trace,
                    status,
                    format_args!("Overflow nread {nread} {}", tail.as_str()),
                );
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
                if let Some(v) = failed_read_field(&e, |r| r.int_val) {
                    out.i32inp = Some(v);
                }
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "{} read error, {}",
                        plan.iface.c_errs_name(),
                        e.message_display()
                    ),
                );
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
                // C assigns its local `data` on any failure (asynRecord.c:1424),
                // uninitialized unless the driver wrote it; that stays unported.
                if let Some(v) = failed_read_field(&e, |r| r.uint_val) {
                    out.ui32inp = Some(v);
                }
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "{} read error, {}",
                        plan.iface.c_errs_name(),
                        e.message_display()
                    ),
                );
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
                if let Some(v) = failed_read_field(&e, |r| r.float_val) {
                    out.f64inp = Some(v);
                }
                // C: reportError(status, "<Iface> read error, %s",
                // pasynUser->errorMessage) — asynRecord.c:1391 Int32, :1429
                // UInt32, :1463 Float64.
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "{} read error, {}",
                        plan.iface.c_errs_name(),
                        e.message_display()
                    ),
                );
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        },
    }
}

/// What a failed register read left in the field C reads into
/// (asynRecord.c:1385, :1421, :1457): the value a parameter's status failed
/// with, or the zero a parameter-library failure writes before failing
/// (asynPortDriver.cpp:304, :359, :386). Other failures leave it as it was.
fn failed_read_field<T: Default>(
    e: &AsynError,
    value: fn(&RequestResult) -> Option<T>,
) -> Option<T> {
    match e {
        AsynError::ParamAccess { .. } => Some(T::default()),
        _ => e.failed_read().and_then(value),
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
    // Every failure, a discarded flush included, left its text on the user.
    if let Err(e) = &res {
        out.note_error(e.message_display());
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
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!("GPIB Universal command {}", e.message_display()),
                );
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibAddressed => {
            // C gpibAddressedCmd (asynRecord.c:1748-1755).
            if let Err(e) = res {
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "Error in GPIB Addressed Command write, {}",
                        e.message_display()
                    ),
                );
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollEnable => {
            // C Serial Poll step 1 (asynRecord.c:1721-1726).
            if let Err(e) = res {
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!("Error in GPIB Serial Poll write, {}", e.message_display()),
                );
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollRead => {
            // C Serial Poll step 2 (asynRecord.c:1729-1735): the response byte
            // lands in SPR, and *either* a failing status *or* a transfer of
            // other than one byte is the error — the record keeps the last SPR
            // in that case, because C only ever wrote through the pointer if the
            // driver moved a byte.
            let (data, status) = octet_outcome(res).into_parts();
            let err = status.err();
            if let Some(byte) = data.first() {
                out.spr = Some(i32::from(*byte));
            }
            if err.is_some() || data.len() != 1 {
                // C's `%s` tail is `pasynUser->errorMessage`, stale after a
                // short read that reported success.
                let detail = out.error_message(plan);
                let status = err.as_ref().map_or(AsynStatus::Success, AsynError::status);
                out.report_error(
                    &plan.trace,
                    status,
                    format_args!("Error in GPIB Serial Poll read, {}", detail.as_str()),
                );
                raise_io_alarm(out, alarm_status::READ_ALARM, AlarmSeverity::Major);
            }
        }
        IoPhase::GpibSerialPollDisable => {
            // C Serial Poll step 3 (asynRecord.c:1738-1744).
            if let Err(e) = res {
                out.report_error(
                    &plan.trace,
                    e.status(),
                    format_args!(
                        "Error in GPIB Serial Poll disable write, {}",
                        e.message_display()
                    ),
                );
                raise_io_alarm(out, alarm_status::WRITE_ALARM, AlarmSeverity::Major);
            }
        }
        // The queue entry was the request. C's callback body is empty for
        // `asynTMOD_NoIO` (asynRecord.c:827), so a request that reached the port
        // reports nothing; the refusal and the queue timeout were already taken
        // above, which is the whole of what a NoI/O cycle can observe.
        IoPhase::NoIo => {}
        IoPhase::NoInterface(name) => {
            out.report_error(
                &plan.trace,
                AsynStatus::Error,
                format_args!("No {name} interface"),
            );
            raise_io_alarm(out, alarm_status::COMM_ALARM, AlarmSeverity::Major);
        }
    }
}

/// Run one cycle's phases against `port` — C `performIO`'s body, one queued
/// request whose transfers call the driver directly (asynRecord.c:1318-1636),
/// [`ProcessJob`]'s callback on whichever port. A failed phase reports and the
/// cycle carries on, as in C.
fn run_io_phases(plan: &mut IoPlan, port: &mut dyn PortCalls) -> IoOutcome {
    let mut out = IoOutcome::entered(plan);
    for phase in io_phases(plan).iter() {
        let op = io_phase_op(plan, phase);
        let res = port.call(&op, io_phase_user(plan, phase));
        return_phase_buffer(plan, op);
        record_phase_result(plan, &mut out, phase, res);
    }
    out
}

/// The bytes [`io_phase_op`] lent `op`, back to `plan` for the next cycle.
fn return_phase_buffer(plan: &mut IoPlan, op: RequestOp) {
    match op {
        RequestOp::OctetWrite { data } | RequestOp::OctetWriteBinary { data } => {
            plan.octet_out = data;
        }
        RequestOp::GpibAddressedCmd { data } => {
            if let Some(GpibCycle::Addressed(frame)) = &mut plan.gpib {
                *frame = data;
            }
        }
        RequestOp::WithDriverAfterQueue(_) => {}
        _ => {}
    }
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
/// [`field@AsynUser::queue_timeout`] of the users the record builds below.
const QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// C `queueTimeoutCallbackProcess` (asynRecord.c:919-927) — the process request
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

/// C `connectDevice` (asynRecord.c:1282-1284 and :1298-1300) — the queue gate
/// refused the option or the EOS readback the attach queues, so that readback
/// never reached the driver.
///
/// The trailing newline is C's format string verbatim (`"queueRequest failed\n"`)
/// and it reaches the field: `reportError` `strncpy`s the formatted buffer into
/// `errs`, so a CA client reading ERRS off a fat `softIoc` attached to a
/// disconnected port gets `queueRequest failed` followed by byte 10.
const CONNECT_READBACK_QUEUE_REFUSED_MSG: &str = "queueRequest failed\n";

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

/// The driver option key C's HOSTINFO field writes (asynRecord.c:1823-1825) —
/// the one option put C queues on a port that is not connected.
const HOSTINFO_OPTION_KEY: &str = "hostinfo";

/// Which queue class an option request belongs to — C's choice between
/// `queueRequest(..., asynQueuePriorityLow, ...)` and
/// `queueRequest(..., asynQueuePriorityConnect, ...)` with
/// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` on the user.
///
/// C makes that choice per *field* (asynRecord.c:566-570), so it is made here
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
    /// The key is compared under [`option_key`]'s rule, the normalisation both halves
    /// of every driver's option interface use, because the spelling that reaches
    /// here is not this crate's to choose: C writes the key capital-I
    /// (`"hostInfo"`, asynRecord.c:1825) and matches it with
    /// `epicsStrCaseCmp` on both driver halves (drvAsynIPPort.c:953, :991).
    /// Comparing the caller's spelling against a lowercase literal would hand a
    /// C- or operator-spelled `hostInfo` the `Normal` queue, and HOSTINFO is the
    /// one option C deliberately queues on a *disconnected* port — see the
    /// waiver in [`AsynRecord::write_option`].
    fn for_key(key: &str) -> Self {
        if is_option_key(key, HOSTINFO_OPTION_KEY) {
            Self::EvenIfNotConnected
        } else {
            Self::Normal
        }
    }
}

/// Which C site queued a request the record put on the port's queue.
///
/// A refusal's diagnostic is a property of the *queueing site*, not of the helper
/// that consumes the result: C reports `"queueRequest failed\n"` where
/// `connectDevice` queues its option and EOS readbacks (asynRecord.c:1281-1285,
/// :1297-1301) and the gate's own `pasynUser->errorMessage` where `special()`
/// queues a set (`:571-578`). The readback helpers are shared by both callers, so
/// the choice travels in from the caller and is applied in exactly one place —
/// [`AsynRecord::report_never_ran`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedBy {
    /// `connectDevice`'s `callbackGetOption` / `callbackGetEos`
    /// (asynRecord.c:1273-1302).
    ConnectReadback,
    /// `special()`'s `callbackSetOption` / `callbackSetEos` / `callbackConnect`
    /// (asynRecord.c:571-578).
    Special,
}

/// What a special request's `AsynUser` is built from — the record's
/// `pasynUser` as `duplicateAsynUser` copies it (asynRecord.c:531-533), taken
/// under the hold so the port half can build one per request off it.
#[derive(Clone)]
struct SpecialUser {
    reason: usize,
    addr: i32,
    drv_user: Option<crate::port::DrvUser>,
    timeout: Option<std::time::Duration>,
    queue: OptionQueue,
}

impl SpecialUser {
    fn user(&self) -> AsynUser {
        let user = AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout_opt(self.timeout)
            .with_drv_user(self.drv_user.clone())
            // C queues every special request, and `connectDevice` its
            // getOption/getEos readbacks, with `QUEUE_TIMEOUT`
            // (asynRecord.c:571-572, :1281, :1297).
            .with_queue_timeout(QUEUE_TIMEOUT);
        match self.queue {
            // C's `asynQueuePriorityLow` (asynRecord.c:564, :1296) — the
            // process read's band, so a special put waits behind a read already
            // queued, as C's FIFO keeps it, instead of the `AsynUser` default.
            OptionQueue::Normal => user.with_priority(crate::port::QueuePriority::Low),
            OptionQueue::EvenIfNotConnected => user.queue_even_if_not_connected(),
        }
    }
}

/// One `asynCallbackSpecial` request — C's `duplicateAsynUser` copy of the
/// record's user and the `callbackMessage` it carries (asynRecord.c:531-537).
/// Written under the record's hold before it is queued, read by the port
/// thread; the buffers keep their capacity, so a put that reuses a request
/// allocates nothing.
struct SpecialRequest {
    what: SpecialWhat,
    user: SpecialUser,
    /// The port has asynOption / asynOctet — what C's `optioniv` / `octetiv`
    /// say when `setOption` / `setEos` test them.
    option: bool,
    octet: bool,
    /// `setOption`'s key and value.
    key: &'static str,
    value: String,
    /// `setEos`'s terminator, `dbTranslateEscape`d.
    eos: Vec<u8>,
    /// C `callbackRequestProcessCallback(..., pasynRec->prio, ...)`.
    prio: epics_libcom_rs::runtime::task::CallbackPriority,
    /// Queued on a blocking port: the result goes to the [`SpecialLane`].
    /// Otherwise it ran in place and the put applies it.
    queued: bool,
}

/// C's `callbackType` (asynRecord.c:834-899).
#[derive(Clone, Copy)]
enum SpecialWhat {
    /// `callbackSetOption`, falling through to `callbackGetOption` (:845-849).
    SetOption,
    /// `connectDevice`'s `callbackGetOption` (:1272-1287).
    GetOption,
    /// `callbackSetEos`, falling through to `callbackGetEos` (:851-855).
    SetEos { output: bool },
    /// `connectDevice`'s `callbackGetEos` (:1288-1302).
    GetEos,
    /// `callbackConnect` (:856-889), for the CNCT the put wrote.
    Connect { want: bool },
}

impl SpecialWhat {
    /// Which C site queued it — see [`QueuedBy`].
    fn queued_by(self) -> QueuedBy {
        match self {
            Self::GetOption | Self::GetEos => QueuedBy::ConnectReadback,
            Self::SetOption | Self::SetEos { .. } | Self::Connect { .. } => QueuedBy::Special,
        }
    }
}

/// An option put's value, read off the record once the request is in hand.
enum OptionValue {
    /// A menu field's choice text.
    Choice(&'static str),
    /// C `sprintf(optionString, "%d", pasynRec->lbaud)` (:1783-1785).
    Lbaud,
    Hostinfo,
}

impl Default for SpecialRequest {
    fn default() -> Self {
        Self {
            what: SpecialWhat::GetOption,
            user: SpecialUser {
                reason: 0,
                addr: 0,
                drv_user: None,
                timeout: None,
                queue: OptionQueue::Normal,
            },
            option: false,
            octet: false,
            key: "",
            value: String::new(),
            eos: Vec::new(),
            prio: epics_libcom_rs::runtime::task::CallbackPriority::Low,
            queued: false,
        }
    }
}

impl SpecialRequest {
    /// The user C hands `queueRequest` for this request, at the priority it
    /// queues it at: `callbackConnect` and HOSTINFO at
    /// `asynQueuePriorityConnect` (asynRecord.c:561-569), `connectDevice`'s
    /// option readback with the sentinel (:1277-1280).
    fn queue_user(&self) -> AsynUser {
        match self.what {
            SpecialWhat::Connect { .. } => AsynUser::new(0)
                .with_addr(self.user.addr)
                .with_priority(crate::port::QueuePriority::Connect)
                .with_queue_timeout(QUEUE_TIMEOUT),
            _ => self.user.user(),
        }
    }

    /// The port half of C `asynCallbackSpecial`, on the port's thread.
    fn run(&self, port: &mut dyn PortCalls) -> SpecialDone {
        let user = &self.user;
        match self.what {
            SpecialWhat::SetOption => {
                // C `setOption`'s `return` leaves `setOption`, not the
                // callback: `getOptions` then returns on `!optioniv` too.
                if !self.option {
                    return SpecialDone::Ran(
                        Some(SpecialReport::NoInterface("asynOption")),
                        Readback::None,
                    );
                }
                let op = RequestOp::SetOption {
                    key: self.key.to_string(),
                    value: self.value.clone(),
                };
                let report = match port.call(&op, user.user()) {
                    Ok(_) => None,
                    Err(e) => Some(SpecialReport::Errs(
                        e.status(),
                        Errs::format(format_args!(
                            "Error setting option, {}",
                            e.message_display()
                        )),
                    )),
                };
                SpecialDone::Ran(report, Readback::Options(read_options(port, user)))
            }
            SpecialWhat::GetOption => {
                SpecialDone::Ran(None, Readback::Options(read_options(port, user)))
            }
            SpecialWhat::SetEos { output } => {
                // C `setEos` returns on `!octetiv`, and `getEos` then posts
                // both fields blank (:1997-1998, `goto post`).
                if !self.octet {
                    return SpecialDone::Ran(
                        Some(SpecialReport::NoInterface(
                            InterfaceType::Octet.c_asyn_name(),
                        )),
                        Readback::Eos(String::new(), String::new()),
                    );
                }
                let eos = self.eos.clone();
                let op = if output {
                    RequestOp::SetOutputEos { eos }
                } else {
                    RequestOp::SetInputEos { eos }
                };
                let report = match port.call(&op, user.user()) {
                    Ok(_) => None,
                    Err(e) => {
                        let which = if output { "output" } else { "input" };
                        Some(SpecialReport::Errs(
                            e.status(),
                            Errs::format(format_args!(
                                "Error setting {which} eos, {}",
                                e.message_display()
                            )),
                        ))
                    }
                };
                let (ieos, oeos) = read_eos(port, user);
                SpecialDone::Ran(report, Readback::Eos(ieos, oeos))
            }
            SpecialWhat::GetEos => {
                let (ieos, oeos) = read_eos(port, user);
                SpecialDone::Ran(None, Readback::Eos(ieos, oeos))
            }
            SpecialWhat::Connect { want } => {
                // The connect user is the record's own, at ADDR, with no
                // sentinel: on a disconnected multi-device port C's
                // `checkPortConnect` refuses it (asynManager.c:1538-1554).
                let user = || AsynUser::new(0).with_addr(self.user.addr);
                let connected = port
                    .call(&RequestOp::GetConnected, user())
                    .map(|r| r.int_val.unwrap_or(0) != 0);
                let op = match (want, connected) {
                    (true, Ok(false)) => Some(("connect", RequestOp::Connect)),
                    (false, Ok(true)) => Some(("disconnect", RequestOp::Disconnect)),
                    // Already in the requested state: no driver call.
                    (_, Ok(_)) => None,
                    (_, Err(_)) => {
                        return SpecialDone::Ran(
                            Some(SpecialReport::Errs(
                                AsynStatus::Error,
                                Errs::format(format_args!("asynCallbackSpecial isConnected error")),
                            )),
                            Readback::None,
                        );
                    }
                };
                let report = match op.map(|(what, op)| (what, port.call(&op, user()))) {
                    Some((what, Err(e))) => Some(SpecialReport::Errs(
                        AsynStatus::Error,
                        Errs::format(format_args!(
                            "asynCallbackSpecial callbackConnect {what}: {}",
                            e.message_display()
                        )),
                    )),
                    _ => None,
                };
                SpecialDone::Ran(report, Readback::None)
            }
        }
    }
}

/// A special request's callbacks — C `asynCallbackSpecial` and
/// `queueTimeoutCallbackSpecial` (asynRecord.c:834-899, :929-938) — over the
/// request the record wrote. The result goes to the record's [`SpecialLane`]
/// before the cell is released, so a put that reuses the cell never meets it.
struct SpecialBody {
    lane: Arc<SpecialLane>,
    request: parking_lot::Mutex<SpecialRequest>,
    /// The result of a request that ran in place, for the put that ran it.
    ran_in_place: parking_lot::Mutex<Option<SpecialDone>>,
}

impl CellBody for SpecialBody {
    fn process(&self, port: &mut dyn PortCalls) {
        let (done, prio, queued) = {
            let request = self.request.lock();
            (request.run(port), request.prio, request.queued)
        };
        if queued {
            self.lane.complete(done, prio);
        } else {
            *self.ran_in_place.lock() = Some(done);
        }
    }

    fn unrun(&self, why: Unrun) {
        let (queued_by, prio) = {
            let request = self.request.lock();
            (request.what.queued_by(), request.prio)
        };
        let e = match why {
            Unrun::Refused(e) => e,
            // Nothing cancels a special request; C's has no cancel either.
            Unrun::TimedOut | Unrun::Canceled => AsynError::QueueTimeout {
                port: String::new(),
            },
        };
        self.lane
            .complete(SpecialDone::NeverRan(e, queued_by), prio);
    }

    fn done(self: Arc<Self>) {}
}

/// A request in the record's pool — see [`AsynRecord::special_request`].
struct SpecialCell {
    cell: Arc<RequestCell>,
    body: Arc<SpecialBody>,
}

/// What a special request did on the port, for
/// [`AsynRecord::complete_special`] to apply under the record's hold.
enum SpecialDone {
    /// It never left the queue: refused by the gate, or timed out in it.
    NeverRan(AsynError, QueuedBy),
    /// It ran: the arm's diagnostic, then the readback it fell through to.
    Ran(Option<SpecialReport>, Readback),
}

enum SpecialReport {
    /// `setOption` / `setEos` on a port without the interface (:1767-1771,
    /// :1957-1961), which also raises COMM/MAJOR.
    NoInterface(&'static str),
    Errs(AsynStatus, Errs),
}

enum Readback {
    None,
    /// `getOptions`: the driver's answer per key.
    Options(HashMap<String, String>),
    /// `getEos`: IEOS and OEOS, escaped.
    Eos(String, String),
}

/// The driver's options under one queued request — C `getOptions`'s
/// `getOption` calls (asynRecord.c:1864-1927). C ignores each `getOption`'s
/// status, so a key the driver does not know is an empty answer.
fn read_options(port: &mut dyn PortCalls, user: &SpecialUser) -> HashMap<String, String> {
    let mut opts = HashMap::new();
    for key in OPTION_READBACK_KEYS {
        let op = RequestOp::GetOption {
            key: (*key).to_string(),
        };
        match port.call(&op, user.user()) {
            Ok(RequestResult {
                option_value: Some(val),
                ..
            }) => {
                opts.insert((*key).to_string(), val);
            }
            // An unknown key, or one the driver answered with no value.
            Ok(_) | Err(_) => {}
        }
    }
    opts
}

/// Both EOS strings from the driver, escaped — C `getEos`
/// (asynRecord.c:1987-2025).
///
/// C seeds both buffers to `""` and overwrites one only when its get succeeds
/// with a non-empty EOS. The escaping is `epicsStrSnPrintEscaped` into an
/// [`EOS_SIZE`]-byte buffer, the inverse of the put's `translate_escape`.
fn read_eos(port: &mut dyn PortCalls, user: &SpecialUser) -> (String, String) {
    let mut read = |op| match port.call(&op, user.user()) {
        Ok(RequestResult {
            data: Some(bytes), ..
        }) if !bytes.is_empty() => crate::escape::escaped_from_raw(&bytes, EOS_SIZE),
        Ok(_) | Err(_) => String::new(),
    };
    let ieos = read(RequestOp::GetInputEos);
    (ieos, read(RequestOp::GetOutputEos))
}

/// The record half of the `asynCallbackSpecial` requests the record queued on
/// a blocking port. C's port thread runs each callback whole, in queue order
/// (asynRecord.c:834-899); here the port half runs on the port's actor
/// ([`SpecialBody`]), which hands its result over, and one drainer at
/// a time applies the results under the record's hold in that order. The hold
/// is never taken on the actor thread: a record holding it may be waiting on
/// that actor.
#[derive(Default)]
struct SpecialLane {
    done: parking_lot::Mutex<SpecialQueue>,
    /// The record the drainer applies results to, set at `add_record`.
    record: parking_lot::Mutex<Option<(epics_base_rs::types::RecordName, AsyncDbHandle)>>,
    /// Held by a test to stop the drainer before it takes the next result.
    #[cfg(test)]
    gate: parking_lot::Mutex<()>,
}

#[derive(Default)]
struct SpecialQueue {
    done: std::collections::VecDeque<SpecialDone>,
    draining: bool,
}

impl SpecialLane {
    /// From the port's actor, as the callback ends: queue the result, and start
    /// the drainer — C `callbackRequest` — unless it is running.
    fn complete(
        self: &Arc<Self>,
        done: SpecialDone,
        prio: epics_libcom_rs::runtime::task::CallbackPriority,
    ) {
        let mut queue = self.done.lock();
        queue.done.push_back(done);
        if std::mem::replace(&mut queue.draining, true) {
            return;
        }
        drop(queue);
        let lane = Arc::clone(self);
        epics_libcom_rs::runtime::task::spawn_blocking_background(prio, move |on| lane.drain(on));
    }

    /// C's `callbackRequest` body: the band runs it, so it takes the band's
    /// proof and the blocking seam cannot reach it.
    fn drain(&self, _on: epics_libcom_rs::runtime::task::OnBand) {
        let record = self.record.lock().clone();
        loop {
            #[cfg(test)]
            let _gate = self.gate.lock();
            let done = {
                let mut queue = self.done.lock();
                match queue.done.pop_front() {
                    Some(done) => done,
                    None => {
                        queue.draining = false;
                        return;
                    }
                }
            };
            let Some((name, db)) = &record else {
                continue;
            };
            db.with_record(name, |rec| {
                if let Some(rec) = rec
                    .as_any_mut()
                    .and_then(|any| any.downcast_mut::<AsynRecord>())
                {
                    rec.complete_special(done);
                }
            });
        }
    }

    /// No result is waiting or being applied.
    #[cfg(test)]
    fn is_idle(&self) -> bool {
        !self.done.lock().draining
    }
}

/// Where C `exceptCallback`'s refresh runs. C runs it on the raising thread,
/// most often the port thread at its default `epicsThreadPriorityMedium`
/// (asynManager.c:2077), and so does this pool. A port whose actor is wedged
/// holds these workers, not the process blocking pool. The priority is not
/// per port: the workers are shared, and re-prioritising one per job would
/// carry a port's priority into the next port's refresh.
static EXCEPT_REFRESH: epics_libcom_rs::runtime::offload::OffloadPool =
    epics_libcom_rs::runtime::offload::OffloadPool::new(
        "asynExcept",
        epics_libcom_rs::runtime::task::ThreadPriority::Medium,
    );

/// A rung of C's `findTracePvt` chain — see [`AsynRecord::trace_rung`]. `addr`
/// is set only with `port`; `port: None` is the global rung.
struct TraceRung<'a> {
    trace: Arc<TraceManager>,
    port: Option<&'a str>,
    addr: Option<i32>,
}

/// The TFIL text C writes when the trace file changed under the record
/// (asynRecord.c:1122).
const TFIL_UNKNOWN: &str = "Unknown";

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

    /// C special()'s `traceMask` from TB0-TB5 (asynRecord.c:418-423). TMSK
    /// itself is `monitorStatus`'s to refresh.
    fn trace_mask_of_bits(&self) -> TraceMask {
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
        TraceMask::from_bits_truncate(mask)
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

    /// C special()'s `traceMask` from TIB0-TIB2 (asynRecord.c:432-434).
    fn io_mask_of_bits(&self) -> TraceIoMask {
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
        TraceIoMask::from_bits_truncate(mask)
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

    /// C special()'s `traceMask` from TINB0-TINB3 (asynRecord.c:444-447).
    fn info_mask_of_bits(&self) -> TraceInfoMask {
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
        TraceInfoMask::from_bits_truncate(mask)
    }

    /// C `findTracePvt` (asynManager.c:538-553) for the record's user: the
    /// device rung on a multi-device port, else the port's, else — a user
    /// connected to no port — the global one. Every trace read and write the
    /// record makes resolves here, so a record whose connect failed still
    /// reads and writes the trace C does; the three mask setters refuse that
    /// user instead ([`Self::mask_rung`]).
    fn trace_rung(&self) -> TraceRung<'_> {
        match self.device_user.device() {
            Some(entry) => TraceRung {
                trace: Arc::clone(entry.handle.trace()),
                port: Some(&self.port),
                addr: entry.handle.dp_addr(self.addr),
            },
            None => TraceRung {
                trace: Arc::clone(crate::manager::PortManager::global().trace_manager()),
                port: None,
                addr: None,
            },
        }
    }

    /// The rung C's `setTraceMask` / `setTraceIOMask` / `setTraceInfoMask`
    /// write: none for a user on no port, which they refuse with "not
    /// connected to port" (asynManager.c:2797-2800, :2839-2842, :2881-2884)
    /// rather than fall to the global rung as `setTraceFile` and
    /// `setTraceIOTruncateSize` do. `special()` drops the status, so nothing
    /// is reported (oracle as12).
    fn mask_rung(&self) -> Option<TraceRung<'_>> {
        Some(self.trace_rung()).filter(|rung| rung.port.is_some())
    }

    /// C `setTraceMask` (asynRecord.c:410, :424).
    fn apply_trace_mask(&mut self, value: TraceMask) {
        if self.mask_rung().is_none() {
            return;
        }
        self.raise_exception(|rec| {
            let rung = rec.trace_rung();
            match (rung.port, rung.addr) {
                (Some(port), Some(addr)) => rung.trace.set_device_trace_mask(port, addr, value),
                (port, _) => rung.trace.set_trace_mask(port, value),
            }
        });
    }

    /// C `setTraceIOMask` (asynRecord.c:427, :435).
    fn apply_trace_io_mask(&mut self, value: TraceIoMask) {
        if self.mask_rung().is_none() {
            return;
        }
        self.raise_exception(|rec| {
            let rung = rec.trace_rung();
            match (rung.port, rung.addr) {
                (Some(port), Some(addr)) => rung.trace.set_device_trace_io_mask(port, addr, value),
                (port, _) => rung.trace.set_trace_io_mask(port, value),
            }
        });
    }

    /// C `setTraceInfoMask` (asynRecord.c:438, :448).
    fn apply_trace_info_mask(&mut self, value: TraceInfoMask) {
        if self.mask_rung().is_none() {
            return;
        }
        self.raise_exception(|rec| {
            let rung = rec.trace_rung();
            match (rung.port, rung.addr) {
                (Some(port), Some(addr)) => {
                    rung.trace.set_device_trace_info_mask(port, addr, value)
                }
                (port, _) => rung.trace.set_trace_info_mask(port, value),
            }
        });
    }

    /// C `setTraceIOTruncateSize` (asynRecord.c:451).
    fn apply_trace_truncate_size(&mut self) {
        let value = self.tsiz as usize;
        self.raise_exception(|rec| {
            let rung = rec.trace_rung();
            // special() drops the status.
            let _ = match (rung.port, rung.addr) {
                (Some(port), Some(addr)) => {
                    rung.trace.set_device_io_truncate_size(port, addr, value)
                }
                (port, _) => rung.trace.set_io_truncate_size(port, value),
            };
        });
    }

    /// Run a setter in `special()` that raises an exception, then C's
    /// `exceptCallback` for it: asynManager calls that on the setter's own
    /// thread, so its `monitorStatus` runs inside `special()` under the
    /// recursive `dbScanLock` (asynRecord.c:911-916), once. The registered
    /// callback stands down while `own_raise` is set — the setter announces
    /// before it returns, on the port actor before its reply — and a foreign
    /// exception it skips in that window is read by the refresh after it.
    ///
    /// The refresh runs only when an exception reached the callback, as C's
    /// does: a set the port refuses announces nothing (`setTraceIOTruncateSize`,
    /// asynManager.c:2963-2975) and leaves every field as it was put.
    fn raise_exception(&mut self, set: impl FnOnce(&Self)) {
        self.own_raise.store(RAISE_OPEN, Ordering::Release);
        set(self);
        if self.own_raise.swap(RAISE_IDLE, Ordering::AcqRel) == RAISE_HEARD {
            self.monitor_status();
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
        let tfil = self.tfil.clone();
        let file = match open_trace_file(&tfil) {
            Ok(file) => file,
            Err(_) => {
                // C reports the path alone — `fopen` leaves no message to splice
                // (asynRecord.c:465-466).
                // `status` is still `special()`'s initial `asynSuccess`.
                self.report_error(
                    AsynStatus::Success,
                    format_args!("Error opening trace file: {tfil}"),
                );
                return;
            }
        };
        // C stores the new `FILE *` in `old.traceFd` BEFORE calling setTraceFile,
        // because setTraceFile fires an exception whose callback runs
        // monitorStatus — which would otherwise see the record's own write as a
        // foreign change and overwrite TFIL with "Unknown" (asynRecord.c:474-475).
        self.old_trace_file_id = file.id();
        self.raise_exception(|rec| {
            let rung = rec.trace_rung();
            match (rung.port, rung.addr) {
                (Some(port), Some(addr)) => rung.trace.set_device_trace_file(port, addr, file),
                (port, _) => rung.trace.set_trace_file(port, file),
            }
        });
    }

    /// Read current trace state from TraceManager into record fields.
    ///
    /// C `monitorStatus` (asynRecord.c:1066-1124) refreshes the trace mask, the
    /// trace I/O mask, the trace info mask (`TINM`/`TINB0..3`), the I/O truncate
    /// size (`TSIZ`, :1100), and `TFIL` — which becomes "Unknown" whenever the
    /// port's trace sink is no longer the one this record installed (:1119-1124).
    /// Answers whether it did.
    ///
    /// The remembered sink is updated here (C updates `old.traceFd` in the same
    /// `if` body, :1120) and by [`Self::apply_trace_file`], which stores the id
    /// of the sink it is about to install *before* installing it, as C does
    /// (:474-475): `setTraceFile` raises an exception whose refresh runs this,
    /// and the record's own write must not read as foreign.
    fn read_trace_state(&mut self) -> bool {
        let snap = {
            let rung = self.trace_rung();
            rung.trace.snapshot(rung.port, rung.addr)
        };
        let file_changed =
            std::mem::replace(&mut self.old_trace_file_id, snap.file_id) != snap.file_id;

        self.tmsk = snap.trace_mask.bits() as i32;
        self.update_trace_bits_from_mask();

        self.tiom = snap.io_mask.bits() as i32;
        self.update_io_bits_from_mask();

        self.tinm = snap.info_mask.bits() as i32;
        self.update_info_bits_from_mask();

        self.tsiz = snap.io_truncate_size as i32;

        if file_changed {
            self.tfil = TFIL_UNKNOWN.to_string();
        }
        file_changed
    }

    /// Subscribe to the port's exceptions — C `exceptCallback`
    /// (asynRecord.c:903-917).
    ///
    /// C registers it with `exceptionCallbackAdd` in `connectDevice`
    /// (asynRecord.c:1269) and takes *every* `asynException`: the body is an
    /// unconditional `monitorStatus(pasynRec)` under `dbScanLock`, so a port
    /// going down, an `asynSetAutoConnect`, an `asynEnable` or an
    /// `asynSetTraceMask` re-imports the readback fields and posts the changed
    /// ones at once, with no `process()`.
    ///
    /// Here too: [`AsyncDbHandle::with_record`] takes the record's hold, runs
    /// [`Self::monitor_status`] and posts its marks under that hold, against
    /// the same `PvtOld` the in-cycle refreshes compare through. It runs on a
    /// blocking worker, not on the thread that raised the exception: that is
    /// the port actor whenever the driver raised it, and `monitor_status`'s
    /// port queries are round trips to that actor. C also takes the lock
    /// before `interruptAccept` (:907, :911); the record is not processed
    /// then, so taking it costs nothing.
    ///
    /// With no database handle or no runtime the callback raises
    /// `status_dirty`, which the next `process()` drains through the same
    /// `monitor_status`.
    fn register_exception_callback(&mut self) {
        self.clear_exception_callback();
        let dirty = Arc::clone(&self.status_dirty);
        let own_raise = Arc::clone(&self.own_raise);
        let immediate = self.async_ctx.clone();
        // On the record's own user, as C's: the `dpCommon` its PORT/ADDR bound,
        // so `asynReport` counts it there and another device's exceptions
        // never reach it.
        self.except_cb = self.device_user.exception_callback_add(move |_| {
            match own_raise.compare_exchange(
                RAISE_OPEN,
                RAISE_HEARD,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) | Err(RAISE_HEARD) => return,
                Err(_) => {}
            }
            let Some((name, db)) = immediate.clone() else {
                dirty.store(true, Ordering::Release);
                return;
            };
            // The exception fires on the thread that raised it, often the port
            // actor, whose state the refresh queries, so it runs elsewhere.
            // On a spawn failure the refresh is lost, as the dirty flag
            // would be for a record that never processes.
            let _ = EXCEPT_REFRESH.spawn(move || {
                db.with_record(&name, |rec| {
                    if let Some(rec) = rec
                        .as_any_mut()
                        .and_then(|any| any.downcast_mut::<AsynRecord>())
                    {
                        rec.monitor_status();
                    }
                });
            });
        });
    }

    /// Remove the trace exception subscription (C `exceptionCallbackRemove`,
    /// asynRecord.c:523,1154,1313). Idempotent.
    fn clear_exception_callback(&mut self) {
        self.except_cb = None;
    }

    /// C `getOptions`'s field writes (asynRecord.c:1864-1927) from the
    /// driver's answers. It is the *only* writer of the option fields from the
    /// driver side, and it runs after every option put as well as on connect,
    /// so those fields show the driver's actual value rather than the
    /// requested one.
    fn apply_option_readback(&mut self, opts: &HashMap<String, String>) {
        // C hands `getOption` a buffer the driver clears at entry
        // (`val[0] = '\0'`, drvAsynSerialPort.c:142, drvAsynIPPort.c:948) and
        // then ignores the returned status (asynRecord.c:1864-1927). So a key the
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
        // (asynRecord.c:1927) copies the buffer whatever the get returned, so a
        // port carrying no `hostinfo` key clears the field.
        self.hostinfo = opt("hostinfo").to_string();
        self.drto = match opt("disconnectOnReadTimeout") {
            "Y" | "Yes" => 2,
            "N" | "No" => 1,
            _ => 0,
        };
    }

    /// C `db_post_events` under the record's lock: mark the post at the first
    /// of `at` (its [`ASYN_POSTS`] entries) not behind the posts already
    /// marked this hold, so marks drain in the order C posted them. Marking a
    /// post twice with nothing between posts it once; the value is read when
    /// the mark drains. A post past every copy of its entry takes the last.
    fn mark(&mut self, at: impl IntoIterator<Item = u32>) {
        let mut last = 0;
        for i in at {
            last = i;
            if i >= self.post_cursor {
                break;
            }
        }
        self.post_marks |= 1 << last;
        self.post_cursor = self.post_cursor.max(last);
    }

    /// C `getOptions`'s bracket around the driver readback: `REMEMBER_STATE`
    /// on the eleven menus (asynRecord.c:1851-1861), then `POST_IF_NEW` on
    /// them and HOSTINFO against its last post (:1928-1943).
    fn get_options(&mut self, opts: &HashMap<String, String>) {
        self.old.options = self.option_values();
        self.apply_option_readback(opts);
        let cur = self.option_values();
        let mut moved = [false; 12];
        for ((moved, cur), old) in moved.iter_mut().zip(cur).zip(&mut self.old.options) {
            *moved = post_if_new(cur, old);
        }
        moved[11] = self.old.hostinfo.post_if_new(&self.hostinfo);
        for (i, moved) in (OPTIONS_AT..).zip(moved) {
            if moved {
                self.mark([i]);
            }
        }
    }

    /// `getOptions`'s menu values in [`ASYN_POSTS`] order; HOSTINFO's slot is 0.
    fn option_values(&self) -> [i32; 12] {
        [
            self.baud, self.lbaud, self.prty, self.sbit, self.dbit, self.mctl, self.fctl,
            self.ixon, self.ixoff, self.ixany, self.drto, 0,
        ]
    }

    /// C `getEos`'s posts (asynRecord.c:2016-2024): IEOS and OEOS when the
    /// driver's EOS differs from the field.
    fn get_eos(&mut self, ieos: String, oeos: String) {
        self.old.ieos.remember(&self.ieos);
        self.old.oeos.remember(&self.oeos);
        self.ieos = ieos;
        self.oeos = oeos;
        if self.old.ieos.post_if_new(&self.ieos) {
            self.mark([EOS_AT]);
        }
        if self.old.oeos.post_if_new(&self.oeos) {
            self.mark([EOS_AT + 1]);
        }
    }

    /// C `special()`'s option arms (asynRecord.c:547-559, :566-578): queue
    /// `callbackSetOption` for `key`.
    ///
    /// The callback always falls through to `getOptions` (:845-849), even when
    /// the set failed, so the option fields are a readback of the driver rather
    /// than a latch of the operator's request: a driver that rounds 9601 to 9600
    /// or rejects the baud outright snaps the field to what it holds.
    ///
    /// C `special()` queues every option put at `asynQueuePriorityLow` but one:
    /// HOSTINFO goes at `asynQueuePriorityConnect` carrying
    /// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`, "Enable changing host:port when
    /// not connected" (:566-570) — the operator's only route to repoint an IP
    /// port aimed at a wrong host, and such a port is disconnected by
    /// definition. The readback runs inside the same request, so under the
    /// same class.
    fn write_option(&mut self, key: &'static str, value: OptionValue) {
        let Some(entry) = self.queue_special() else {
            return;
        };
        let special = self.special_request(
            &entry.handle,
            SpecialWhat::SetOption,
            OptionQueue::for_key(key),
        );
        {
            use std::fmt::Write;
            let mut request = special.body.request.lock();
            request.key = key;
            request.value.clear();
            match value {
                OptionValue::Choice(choice) => request.value.push_str(choice),
                OptionValue::Lbaud => {
                    let _ = write!(request.value, "{}", self.lbaud);
                }
                OptionValue::Hostinfo => request.value.push_str(&self.hostinfo),
            }
        }
        self.queue_port_request(entry.handle, special);
    }

    /// C `queueRequest` of an `asynCallbackSpecial` request, and the callback.
    ///
    /// On a blocking port with a database the request is queued on the port
    /// as a [`RequestOp::Cell`] and the put returns: C's `queueRequest`
    /// takes `asynManagerLock` under the record lock, links the request and
    /// signals the port thread (asynManager.c:1542-1628), and that is all this
    /// does under the hold. The actor runs the port half and hands the result
    /// to the [`SpecialLane`]. Anything else runs it here, as C's
    /// `queueRequest` runs a synchronous port's callback before it returns.
    ///
    /// C's `queueRequest` refuses before anything is queued, so its refusal is
    /// reported here, under the hold and before the put returns
    /// (asynRecord.c:571-578, :1281-1285, :1297-1301).
    /// Every special request queued so far has run and been applied.
    #[cfg(test)]
    fn specials_settled(&self) -> bool {
        self.special_cells.iter().all(|s| !s.cell.is_outstanding()) && self.special_lane.is_idle()
    }

    fn queue_port_request(&mut self, handle: PortHandle, special: SpecialCell) {
        let queued = handle.can_block() && self.async_ctx.is_some();
        let (user, queued_by) = {
            let mut request = special.body.request.lock();
            request.queued = queued;
            (request.queue_user(), request.what.queued_by())
        };
        if let Err(refusal) = handle.queue_gate(&user) {
            self.report_queue_refused(refusal.status(), format_args!("{refusal}"), queued_by);
            return;
        }
        if queued {
            if let Err(refusal) = special.cell.queue(&user) {
                self.report_queue_refused(refusal.status(), format_args!("{refusal}"), queued_by);
            }
            return;
        }
        let done = match handle.run_cell(&special.cell, &user) {
            Ok(()) => special.body.ran_in_place.lock().take(),
            Err(e) => Some(SpecialDone::NeverRan(e, queued_by)),
        };
        if let Some(done) = done {
            self.complete_special(done);
        }
    }

    /// A free request from the record's pool on `handle`'s port, written with
    /// `what` — C `duplicateAsynUser` + `memMalloc(callbackMessage)`, whose
    /// freelists hand back a released user and message (asynManager.c:1184-1199,
    /// :1271-1281). The pool grows only while every request in it is
    /// outstanding, as C's freelist does; a request on another port is dropped.
    fn special_request(
        &mut self,
        handle: &PortHandle,
        what: SpecialWhat,
        queue: OptionQueue,
    ) -> SpecialCell {
        self.special_cells.retain(|s| handle.owns_cell(&s.cell));
        let free = self
            .special_cells
            .iter()
            .position(|s| !s.cell.is_outstanding());
        let special = match free {
            Some(i) => SpecialCell {
                cell: Arc::clone(&self.special_cells[i].cell),
                body: Arc::clone(&self.special_cells[i].body),
            },
            None => {
                let body = Arc::new(SpecialBody {
                    lane: Arc::clone(&self.special_lane),
                    request: parking_lot::Mutex::default(),
                    ran_in_place: parking_lot::Mutex::default(),
                });
                let cell = handle.new_cell(body.clone());
                self.special_cells.push(SpecialCell {
                    cell: Arc::clone(&cell),
                    body: Arc::clone(&body),
                });
                SpecialCell { cell, body }
            }
        };
        {
            use crate::interfaces::InterfaceType as Registry;
            let mut request = special.body.request.lock();
            request.what = what;
            request.user = self.special_user(queue);
            request.option = handle.has_interface(Registry::Option);
            request.octet = handle.has_interface(Registry::Octet);
            request.prio = self.callback_priority;
        }
        special
    }

    /// The record half of C `asynCallbackSpecial` (asynRecord.c:834-899): the
    /// arm's report, the readback it falls through to, then `monitorStatus`
    /// (:897), which every arm reaches — an arm's error `break` leaves the
    /// switch, not the function. A request that never ran reports and stops:
    /// `queueTimeoutCallbackSpecial` (:929-938) and a refused `queueRequest`
    /// (:571-578) run no `monitorStatus`.
    fn complete_special(&mut self, done: SpecialDone) {
        let (report, readback) = match done {
            SpecialDone::NeverRan(e, queued_by) => {
                self.report_never_ran(&e, queued_by);
                return;
            }
            SpecialDone::Ran(report, readback) => (report, readback),
        };
        match report {
            Some(SpecialReport::NoInterface(name)) => self.report_no_interface(name),
            Some(SpecialReport::Errs(status, errs)) => self.report_errs(status, errs),
            None => {}
        }
        match readback {
            Readback::None => {}
            Readback::Options(opts) => self.get_options(&opts),
            Readback::Eos(ieos, oeos) => self.get_eos(ieos, oeos),
        }
        self.monitor_status();
    }

    /// C `reportError` (asynRecord.c:2028-2049): **the** single owner of every
    /// ERRS write.
    ///
    /// C's shape, and the whole point of it: write the field, then
    /// `db_post_events(errs, DBE_VALUE|DBE_LOG)` iff the text actually changed
    /// (:2044-2048). ERRS is not `pp(TRUE)` (asynRecord.dbd:622-627), and the
    /// record's own writes are not puts, so nothing else posts it — a diagnostic
    /// written straight into the field reached the operator's screen only if some
    /// *other* field happened to be posted for an unrelated reason. Every refusal,
    /// option error, EOS error, connect error and trace-file error was invisible
    /// to a CA client monitoring ERRS (R15-49).
    ///
    /// The change guard is C's, not an optimisation: an unchanged ERRS must not
    /// re-post, or a record retrying a down port every second would fire a monitor
    /// every second with text the client already has.
    ///
    /// C prints the text on the record's trace rung first, at ASYN_TRACE_ERROR,
    /// unless `status` is success or timeout (:2039-2042) — under the hold, as
    /// C does under `dbScanLock`, and without allocating.
    fn report_error(&mut self, status: AsynStatus, msg: std::fmt::Arguments<'_>) {
        self.report_errs(status, Errs::format(msg));
    }

    /// A raw ERRS store, as C's `reportError` writes `pasynRec->errs`
    /// (asynRecord.c:2037); it posts nothing.
    fn store_errs(&mut self, value: EpicsValue) -> CaResult<()> {
        self.errs = match &value {
            EpicsValue::CharArray(bytes) => {
                // Drop the `ERR_SIZE` NUL padding `get_field` adds: C's buffer
                // is fixed-size but the text in it ends at the first NUL
                // (`:2037`).
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                Errs::from_bytes_lossy(&bytes[..end])
            }
            EpicsValue::String(s) => Errs::from_bytes_lossy(s.as_bytes()),
            // ERRS is SPC_NOMOD (asynRecord.c:962): only the record's own
            // text is stored here.
            _ => return Err(CaError::TypeMismatch("ERRS".into())),
        };
        Ok(())
    }

    /// [`Self::report_error`] of a text already formatted, for a message that
    /// reads the record itself.
    fn set_errs(&mut self, errs: Errs) {
        // C's `strncmp(errs, old.errs, ERR_SIZE-1)` (:2044), which is the same
        // comparison because C updates `old.errs` only on the branch that posts.
        if std::mem::replace(&mut self.errs, errs) == self.errs {
            return;
        }
        // C posts here and then, under the record's lock: from `process()`
        // ahead of `recGblSetSevr` and the stamp, from `special()` ahead of
        // dbPut's own post. Every caller holds the record, so the mark is
        // drained under the same hold. Two moves in one hold post once: the
        // value is read when the mark is emitted.
        self.mark(ERRS_AT);
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
        self.set_errs(Errs::EMPTY);
    }

    /// C's `state == stateNoDevice` refusal (asynRecord.c:356-357): a record with
    /// no port refuses the transfer in `process()`, before `performIO` — and so
    /// before the interface dispatch — because there is no port to ask about
    /// interfaces.
    ///
    /// The refusal is an alarm, not just a message: C falls out of that branch
    /// into `recGblSetSevr(pasynRec, STATE_ALARM, MINOR_ALARM)` (asynRecord.c:361)
    /// on *every* such process, so a record whose port never came up sits in
    /// STATE/MINOR rather than NO_ALARM. Staged in `io_alarm` and committed by
    /// `check_alarms` on this cycle, like every other record alarm.
    fn report_not_connected(&mut self) {
        self.report_error(AsynStatus::Success, format_args!("Not connect to a port"));
        self.io_alarm = Some((alarm_status::STATE_ALARM, AlarmSeverity::Minor));
    }

    /// C's "the port does not implement this interface" refusal of a special:
    /// report and raise COMM_ALARM/MAJOR_ALARM, do no I/O — `setEos` on
    /// `!octetiv` (:1957-1960) and `setOption` on `!optioniv` (:1767-1770).
    /// The process cycle's are [`IoPhase::NoInterface`].
    ///
    /// The severity is staged in `io_alarm` and committed by `check_alarms` on
    /// this cycle, like every other record alarm.
    fn report_no_interface(&mut self, asyn_name: &str) {
        self.report_error(AsynStatus::Error, format_args!("No {asyn_name} interface"));
        self.io_alarm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Major));
    }

    /// C `queueTimeoutCallbackSpecial` (asynRecord.c:929-938): the option / EOS /
    /// connect-readback request waited out `QUEUE_TIMEOUT` and was removed from
    /// the queue. C reports the text, returns the record to `stateIdle` and frees
    /// the request — and, unlike the process timeout, raises **no** severity, so
    /// this must not go through `report_no_interface`'s alarm-raising shape.
    ///
    /// The single owner of that outcome for every request the record's
    /// [`Self::special_user`] builds.
    fn report_special_queue_timeout(&mut self) {
        self.report_error(
            AsynStatus::Error,
            format_args!("{SPECIAL_QUEUE_TIMEOUT_MSG}"),
        );
    }

    /// The outcome of a request the record queued that **never ran** — the single
    /// owner of every no-callback exit C has, and of the answer every consumer of
    /// a queued result gives when the result says the driver was never reached.
    ///
    /// - The port's queue gate refused it: C's `queueRequest` returned
    ///   non-success. `special()` writes `pasynUser->errorMessage` to ERRS and
    ///   frees the user (asynRecord.c:571-578) — `asynCallbackSpecial` never runs,
    ///   so no option or EOS is written, no connect is attempted, no readback, and
    ///   no `monitorStatus` tail. `connectDevice` reports its own literal instead
    ///   ([`CONNECT_READBACK_QUEUE_REFUSED_MSG`], :1282-1284, :1298-1300), which
    ///   is why the text comes in from the caller as [`QueuedBy`].
    /// - It waited out `QUEUE_TIMEOUT`: `queueTimeoutCallbackSpecial` runs
    ///   instead of the callback (:929-938), reporting its own text. Both queueing
    ///   sites hand that callback to `duplicateAsynUser` (:1274-1275, :1291-1292,
    ///   :531-533), so the timeout text does *not* vary with the caller.
    ///
    /// Every [`SpecialDone::NeverRan`] lands here through
    /// [`Self::complete_special`], which then runs no tail: an arm that reported
    /// the refusal and carried on would drive a readback, `monitorStatus` and its
    /// posts off a request the port never accepted.
    fn report_never_ran(&mut self, e: &AsynError, queued_by: QueuedBy) {
        if e.is_queue_timeout() {
            self.report_special_queue_timeout();
        } else {
            self.report_queue_refused(
                e.status(),
                format_args!("{}", e.message_display()),
                queued_by,
            );
        }
    }

    /// [`Self::report_never_ran`]'s `queueRequest` refusal, its text `message`.
    fn report_queue_refused(
        &mut self,
        status: AsynStatus,
        message: std::fmt::Arguments<'_>,
        queued_by: QueuedBy,
    ) {
        match queued_by {
            QueuedBy::Special => self.report_special_refused(status, message),
            QueuedBy::ConnectReadback => self.report_error(
                AsynStatus::Error,
                format_args!("{CONNECT_READBACK_QUEUE_REFUSED_MSG}"),
            ),
        }
    }

    /// C `special()`'s refused `queueRequest` (asynRecord.c:573-575): its own
    /// text, then `pasynUser->errorMessage` — the manager's, e.g. "port X not
    /// connected" — which is the one ERRS keeps. No severity: C's
    /// `reportError` raises none.
    fn report_special_refused(
        &mut self,
        status: AsynStatus,
        error_message: std::fmt::Arguments<'_>,
    ) {
        self.report_error(status, format_args!("queueRequest failed for special."));
        self.report_error(status, error_message);
    }

    /// C `special()`'s `queueRequest` for the arms it hands to
    /// `asynCallbackSpecial` — CNCT, the options, IEOS/OEOS (asynRecord.c:
    /// 528-578). A record on no port is refused there with the manager's own
    /// text (asynManager.c:1528-1531), and no callback, so no `monitorStatus`,
    /// runs.
    fn queue_special(&mut self) -> Option<crate::registry::PortEntry> {
        match self.device_user.device() {
            Some(entry) => Some(entry.clone()),
            None => {
                // C `queueRequest`'s `asynError` for a user on no port
                // (asynManager.c:1528-1531).
                self.report_special_refused(
                    AsynStatus::Error,
                    format_args!("asynManager::queueRequest asynUser not associated with a port"),
                );
                None
            }
        }
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
        let binding = self.device_user.device().map(|entry| IoIntrBinding {
            handle: entry.handle.clone(),
            iface: InterfaceType::from_u16(self.iface as u16),
            addr: self.addr,
            reason: self.resolved_reason,
            ui32mask: self.ui32mask,
            drv_user: self.drv_user.clone(),
        });
        if let Err(refusal) = self.io_intr.rebind(binding) {
            self.report_register_refusal(refusal);
        }
    }

    /// C `registerInterrupts`' report of its failure (asynRecord.c:617-653).
    fn report_register_refusal(&mut self, refusal: RegisterRefusal) {
        println!("{}", self.register_refusal_line(refusal));
    }

    fn register_refusal_line(&mut self, refusal: RegisterRefusal) -> String {
        match refusal {
            RegisterRefusal::NoInterface(msg) => {
                self.report_error(AsynStatus::Error, format_args!("{msg}"))
            }
            RegisterRefusal::Refused(e) => {
                self.device_user
                    .set_error_message(ErrorMessage::format(format_args!(
                        "{}",
                        e.message_display()
                    )))
            }
        }
        // Either way C prints the user's errorMessage, which "No asynInt32
        // interface" never wrote: that arm reprints whatever the last call left.
        format!(
            "{} registerInterrupts {}",
            self.record_name(),
            self.device_user.error_message()
        )
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
        // C calls `dbPutField` inline under its recursive `dbScanLock`; this
        // hold is not recursive, so the put runs on the putting thread as soon
        // as the hold drops, before the put that reached `special()` returns.
        // No database (a record built outside an IOC): the registration is
        // cancelled above; there is no SCAN field to put.
        self.cancel_scan_owed = self.cancel_scan.is_some();
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
        self.device_user
            .device()
            .is_some_and(|entry| entry.handle.has_interface(iface))
    }

    /// C `special()`'s IEOS/OEOS arm (asynRecord.c:560-562): queue
    /// `callbackSetEos`, which falls through to `getEos` (:851-855), so
    /// IEOS/OEOS show the driver's EOS, not the requested one, whether or not
    /// the set succeeded.
    fn write_eos(&mut self, output: bool) {
        let Some(entry) = self.queue_special() else {
            return;
        };
        let special = self.special_request(
            &entry.handle,
            SpecialWhat::SetEos { output },
            OptionQueue::Normal,
        );
        {
            let field = if output { &self.oeos } else { &self.ieos };
            let mut request = special.body.request.lock();
            request.eos.clear();
            translate_escape_into(field.as_bytes(), &mut request.eos);
        }
        self.queue_port_request(entry.handle, special);
    }

    /// Refresh CNCT from the port's *transport* state — the single owner of
    /// that field's value.
    ///
    /// C parity: CNCT is a readback, not a latch. `monitorStatus`
    /// (asynRecord.c:1090-1094) assigns it from `pasynManager->isConnected` on
    /// every cycle, and `isConnected` fails (→ CNCT=0) when the record is bound
    /// to no port. It says nothing about whether *this record* found its port —
    /// that is PCNCT (asynRecord.c:519-527, `connectDevice` /
    /// `pasynManager->disconnect`). Driving CNCT anywhere else is what gave the
    /// field two meanings: `connect_device` used to latch `cnct = 1` on a
    /// successful *attach*, so a registered-but-unconnected port (noAutoConnect,
    /// or a link that dropped) reported "Connected" on a dead wire.
    fn refresh_connected_state(&mut self) {
        let connected = self.device_user.is_connected().unwrap_or(false);
        self.cnct = i32::from(connected);
    }

    /// C `monitorStatus` (asynRecord.c:1042-1140) on the scan thread: re-import
    /// the trace masks and re-read AUCT / CNCT / ENBL from the port, so every
    /// readback field shows the port's current state.
    ///
    /// The single owner of that refresh. C runs it from `connectDevice`
    /// (:1271,:1319), from every completed callback (:897), and from
    /// `exceptCallback` (:914), each under the record's lock; so does this
    /// port, the last through [`Self::register_exception_callback`].
    ///
    /// A failing query lands as 0, exactly as C's `else` branches do
    /// (:1089,:1094,:1099).
    fn monitor_status(&mut self) {
        // C `REMEMBER_STATE` on the fields another thread can move
        // (:1045-1064): they post only if this refresh moves them.
        let cur = self.status_values();
        for i in (0..=16).chain([18, 19, 23]) {
            self.old.status[i] = cur[i];
        }
        let file_changed = self.read_trace_state();
        // C queries the manager in this order — `isAutoConnect` (:1085),
        // `isConnected` (:1090), `isEnabled` (:1095) — and each failure
        // overwrites the same `pasynUser->errorMessage`. The order is therefore
        // observable: on a record bound to no device the *last* of the three is
        // the text `special()` splices into "connectDevice failed: %s" (:515).
        let auto = self.device_user.is_auto_connect().unwrap_or(false);
        self.auct = i32::from(auto);
        self.refresh_connected_state();
        let enabled = self.device_user.is_enabled().unwrap_or(false);
        self.enbl = i32::from(enabled);

        // The `POST_IF_NEW` block (:1102-1139), TFIL on C's own test (:1119).
        let cur = self.status_values();
        let mut moved = [false; 30];
        for ((moved, cur), old) in moved.iter_mut().zip(cur).zip(&mut self.old.status) {
            *moved = post_if_new(cur, old);
        }
        moved[17] = file_changed;
        moved[22] = self.old.drvinfo.post_if_new(&self.drvinfo);
        for (i, moved) in (0..).zip(moved) {
            if moved {
                self.mark(STATUS_AT.map(|at| at + i));
            }
        }
    }

    /// `monitorStatus`'s integer values in [`ASYN_POSTS`] order; TFIL's and
    /// DRVINFO's slots are 0.
    fn status_values(&self) -> [i32; 30] {
        [
            self.tmsk,
            self.tb0,
            self.tb1,
            self.tb2,
            self.tb3,
            self.tb4,
            self.tb5,
            self.tiom,
            self.tib0,
            self.tib1,
            self.tib2,
            self.tinm,
            self.tinb0,
            self.tinb1,
            self.tinb2,
            self.tinb3,
            self.tsiz,
            0,
            self.auct,
            self.cnct,
            self.pcnct,
            self.reason,
            0,
            self.enbl,
            self.octetiv,
            self.optioniv,
            self.gpibiv,
            self.i32iv,
            self.ui32iv,
            self.f64iv,
        ]
    }

    /// Attempt to connect to the port specified in the PORT field.
    ///
    /// C `connectDevice` (asynRecord.c:1142-1321). *Why* it failed does not
    /// come back on the return: the manager left that in the record user's
    /// buffer ([`crate::manager::DeviceUser::error_message`]), and the failure
    /// tail below runs `monitorStatus` over the same buffer — which is why
    /// `special()` reports the `isEnabled` text and not the connect text
    /// (asynRecord.c:515). This function's own `"Connect error, status=%d, %s"`
    /// (asynRecord.c:1160) is what stays in ERRS for the init and PCNCT paths,
    /// which add no wrapper of their own.
    fn connect_device(&mut self) -> Result<(), crate::manager::DeviceError> {
        // C `connectDevice` opens with `resetError` (asynRecord.c:1151): the
        // previous connection's diagnostic is cleared *before* the attempt, and
        // whatever this attempt reports (`"Connect error…"`, `"Error in
        // asynDrvUser->create()"`) is what stays in ERRS.
        self.reset_error();

        // C detaches before it attaches (asynRecord.c:1152-1154:
        // `exceptionCallbackRemove` + `pasynManager->disconnect`). Two things
        // follow, and the second is the observable one: the manager's "already
        // connected to device" arm is unreachable from a record, and every
        // failure below is queried against a user that is *already* unbound, so
        // the `monitorStatus` in the tail fails all three of its `is*` calls
        // exactly as C's does.
        self.clear_exception_callback();
        self.device_user.disconnect();

        match self.device_user.connect_device(Some(&self.port), self.addr) {
            Ok(entry) => {
                // C asks for asynCommon before anything else and reports with
                // `connectDevice`'s `asynSuccess` (asynRecord.c:1163-1171); bound
                // to a shut-down port, that lookup is what fails.
                if let Err(e) = self.device_user.find_common() {
                    let errs = Errs::format(format_args!(
                        "findInterface common: {}",
                        self.device_user.error_message()
                    ));
                    self.report_errs(AsynStatus::Success, errs);
                    return self.connect_device_bad(e);
                }
                // C's process request is made once, before any cycle queues it
                // (asynRecord.c:306-308); here it belongs to the port.
                self.process_cell_on(&entry.handle);
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
                        match entry.handle.drv_user_create_on_put(&req) {
                            Ok(info) => {
                                self.resolved_reason = info.reason;
                                self.reason = info.reason as i32;
                                self.drv_user = info.drv_user;
                            }
                            Err(e) => {
                                // C reports the bare literal — the driver's own text
                                // reaches the operator through the trace file, not
                                // ERRS (asynRecord.c:1255).
                                //
                                // …and assigns NEITHER reason. C writes
                                // `pasynRec->reason = pasynUser->reason` only on
                                // success (asynRecord.c:1253), and a failed
                                // `create` leaves `pasynUser->reason` untouched
                                // too. Zeroing the resolved reason here pointed
                                // every subsequent read and write at parameter 0
                                // while REASON still read back what the operator
                                // set — the record silently operating on the
                                // wrong parameter (R18-89).
                                self.report_error(
                                    e.status(),
                                    format_args!("Error in asynDrvUser->create()"),
                                );
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
                        self.report_error(
                            AsynStatus::Error,
                            format_args!("asynDrvUser not supported but drvInfo not blank"),
                        );
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

                // C's first `monitorStatus` (:1271), ahead of the readbacks it
                // queues: everything the attach changed — the interface flags,
                // REASON, DRVINFO, the port's ENBL/AUCT/CNCT — posts against
                // `old`, before BAUD/IEOS (R14-47).
                self.monitor_status();

                // C queues the option readback only on a port with asynOption,
                // at `asynQueuePriorityConnect` carrying
                // `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (asynRecord.c:
                // 1272-1287), so a record attaching to a *down* serial or IP port
                // still shows its BAUD/PRTY/HOSTINFO/… — and the EOS readback only
                // on a port with asynOctet, at `asynQueuePriorityLow` with no
                // waiver (:1288-1302), so on a disconnected port the gate refuses
                // it and IEOS/OEOS stay blank. Each completes as its own
                // `asynCallbackSpecial`, posts and `monitorStatus` included.
                if self.optioniv != 0 {
                    let special = self.special_request(
                        &entry.handle,
                        SpecialWhat::GetOption,
                        OptionQueue::EvenIfNotConnected,
                    );
                    self.queue_port_request(entry.handle.clone(), special);
                }
                if self.octetiv != 0 {
                    let special = self.special_request(
                        &entry.handle,
                        SpecialWhat::GetEos,
                        OptionQueue::Normal,
                    );
                    self.queue_port_request(entry.handle.clone(), special);
                }

                // Attached to the port (C `connectDevice` sets PCNCT=1,
                // asynRecord.c:1305).
                self.pcnct = 1;
                // C `connectDevice` ends in `monitorStatus` (asynRecord.c:1271,
                // :1319): the trace masks and the port's actual enable /
                // auto-connect / connect state are *queried* into TMSK…/ENBL /
                // AUCT / CNCT. They must not be forced to 1, which would discard
                // a user who configured ENBL=0 or a port registered
                // noAutoConnect, and would claim a live wire on a port whose
                // transport is down. `monitorStatus` posts what it re-imported
                // against `old`.
                self.monitor_status();
                // C `connectDevice` adds `exceptCallback` (asynRecord.c:1269) so
                // any later exception on the port — a trace change, a dropped
                // link, an enable — refreshes the readback fields.
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
            Err(e) => {
                // C reports its own diagnostic *before* the tail below
                // (asynRecord.c:1160-1161, then `goto bad`): `monitorStatus`
                // overwrites the manager buffer this reads.
                self.report_connect_error();
                self.connect_device_bad(e)
            }
        }
    }

    /// C `connectDevice`'s `bad:` → `done:` tail (asynRecord.c:1309-1320): unbind
    /// whatever the attempt bound, then the same `cancelIOInterruptScan` +
    /// `monitorStatus` the success path takes, so the operator sees PCNCT and
    /// CNCT go to 0 on the record that just lost its port.
    fn connect_device_bad(
        &mut self,
        e: crate::manager::DeviceError,
    ) -> Result<(), crate::manager::DeviceError> {
        self.clear_exception_callback();
        self.device_user.disconnect();
        self.pcnct = 0;
        self.monitor_status();
        self.publish_io_intr_binding();
        Err(e)
    }

    /// C `connectDevice`'s own failure diagnostic (asynRecord.c:1159-1162):
    /// `reportError(status, "Connect error, status=%d, %s", status,
    /// pasynUser->errorMessage)`, where `status` is the numeric `asynStatus`
    /// the manager returned (`asynError` = 3 for every `connectDevice` failure,
    /// asynManager.c:1331-1348).
    ///
    /// Reads the buffer the manager just wrote, so it must run before the
    /// `bad:` → `done:` tail re-writes it.
    fn report_connect_error(&mut self) {
        let errs = Errs::format(format_args!(
            "Connect error, status={}, {}",
            AsynStatus::Error as i32,
            self.device_user.error_message()
        ));
        self.report_errs(AsynStatus::Error, errs);
    }

    /// [`Self::report_error`] of a text already formatted, for a message that
    /// reads the record itself.
    fn report_errs(&mut self, status: AsynStatus, errs: Errs) {
        let rung = self.trace_rung();
        let reason = self.resolved_reason as i32;
        let name = self.record_name();
        trace_report(
            &rung.trace,
            rung.port,
            rung.addr,
            reason,
            name,
            status,
            &errs,
        );
        self.set_errs(errs);
    }

    /// The I/O timeout this cycle carries — the single owner of TMOT's
    /// translation into `AsynUser::timeout`.
    ///
    /// C `asynCallbackProcess` assigns the field verbatim:
    /// `pasynUser->timeout = pasynRec->tmot` (asynRecord.c:818). The `double`
    /// is three-valued, and the transports read all three:
    ///
    /// * `> 0` — bounded wait.
    /// * `== 0` — non-blocking poll. `drvAsynIPPort.c:792-793` computes
    ///   `readPollmsec = (int)(timeout * 1000.0)` and floors a zero to a 1 ms
    ///   poll; `drvAsynSerialPort.c:902-905` sets `VMIN=0, VTIME=0`, the termios
    ///   "return whatever is already buffered" read.
    /// * `< 0` — wait forever (`readPollmsec = -1`, `drvAsynIPPort.c:794`).
    ///
    /// The port used to substitute a 1 s wait for BOTH non-positive cases, so an
    /// operator asking for a poll got a one-second block on a silent device.
    /// `tmot >= 0` now passes through verbatim: `Duration::ZERO` reaches the
    /// transports, whose own zero handling already reproduces C's
    /// (`ip_port::socket_poll_timeout` floors it to the same 1 ms;
    /// `serial_port::duration_to_poll_ms` yields the same 0 ms `poll()`).
    ///
    /// `tmot < 0` passes through as well, as `None` — the operator asked to
    /// wait forever and every driver wait built from it is unbounded (DRV-42).
    /// It used to fall back to the bounded [`crate::user::DEFAULT_TIMEOUT`], so
    /// a record told to wait forever timed out after a second and raised a
    /// READ/WRITE alarm C never raises. A non-finite or out-of-range TMOT still
    /// takes that fallback — C's `(int)` cast of such a value is undefined
    /// behaviour, so there is no C semantics to port.
    ///
    /// The mapping itself belongs to [`crate::user::timeout_from_secs`], the
    /// crate-wide owner of `f64 seconds` → `AsynUser::timeout`; this method only
    /// names TMOT as the source.
    fn io_timeout(&self) -> Option<std::time::Duration> {
        crate::user::timeout_from_secs(self.tmot)
    }

    /// What the `asynUser` of an option, EOS or readback request is built from.
    ///
    /// C queues the option callback on a `duplicateAsynUser` of the record's own
    /// `pasynUser` (asynRecord.c:531-533), and `duplicateAsynUser` copies
    /// `timeout` (asynManager.c:1227). The record's `pasynUser->timeout` is TMOT
    /// (asynRecord.c:818), so an RFC 2217 negotiation triggered by a BAUD/PRTY/…
    /// put is bounded by TMOT — the record's own timeout, not a constant private
    /// to the COM layer. ADDR rides along because C's option callback runs on the
    /// device the record is connected to.
    ///
    /// (C assigns `pasynUser->timeout = tmot` inside `asynCallbackProcess`, so an
    /// option put made before the record has ever processed inherits the 1 s
    /// `createAsynUser` default instead. That lazy assignment is not modelled:
    /// TMOT is the record's timeout from the first put onwards.)
    fn special_user(&self, queue: OptionQueue) -> SpecialUser {
        SpecialUser {
            reason: self.resolved_reason,
            addr: self.addr,
            drv_user: self.drv_user.clone(),
            timeout: self.io_timeout(),
            queue,
        }
    }

    /// Clamp the operator's requested transfer sizes into the record fields —
    /// the single owner of NOWT/NRRD's effective value.
    ///
    /// C `performOctetIO` does not clamp into locals: it writes the clamped
    /// value back into the record itself — `nowt = omax` when a Binary write
    /// asks for more than the output buffer holds (asynRecord.c:1499) and
    /// `nrrd = inlen` when the read asks for more than the IFMT-selected input
    /// buffer holds (:1513) — and `monitor()` `POST_IF_NEW`s both (:1020,1022).
    /// The fields therefore show the *effective* transfer sizes, not the
    /// requested ones. Called from [`Self::fill_io_plan`] on the scan thread
    /// before the plan snapshot, so the inline and queued runners see
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
    /// it writes (`dbAccess.c:1365-1368`) with `nNew` = the element count that
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
    /// stale value, and [`Self::fill_octet_output`] then sent `bout[..NOWT]`
    /// — 80 bytes on the wire where C sends 120.
    fn put_array_field(&mut self, field: AsynArrayField, data: Vec<u8>) {
        match field {
            AsynArrayField::Bout => {
                self.nowt = write_head(&mut self.bout, &data, self.omax);
            }
            AsynArrayField::Binp => {
                self.nord = write_head(&mut self.binp, &data, self.imax);
            }
        }
    }

    /// Build the octet output payload by `OFMT`, mirroring
    /// `asynRecord.c:1486-1502`:
    ///   - ASCII  -> `dbTranslateEscape(AOUT)`: the AOUT string with escape
    ///     sequences (`\r\n` -> CRLF) translated.
    ///   - Hybrid -> `dbTranslateEscape(BOUT read as a C string)`: the binary
    ///     output buffer, escape-translated (stops at the first NUL).
    ///   - Binary -> raw BOUT, `NOWT` bytes, no translation.
    ///
    /// ASCII/Hybrid emit the full translated buffer; only Binary is
    /// length-bounded by `NOWT`. Previously the record sent raw AOUT
    /// for both ASCII and Hybrid (ASCII shipped literal backslashes, Hybrid
    /// ignored BOUT) and clamped every mode by `NOWT`.
    ///
    /// R18-84: `OMAX` bounds the payload in **every** mode, not just Binary.
    /// C never sends from anywhere but an `omax`-sized allocation — `optr` for
    /// Binary and `pasynRecPvt->outbuff` for the two translated modes, both
    /// `callocMustSucceed(pasynRec->omax, ...)` at asynRecord.c:295-300 — so
    /// the physical size of the buffer is the bound and it is uniform. Binary
    /// additionally clamps `NOWT` into the field (`:1499`) because that count
    /// is operator-visible; the truncation below is what makes the bound hold
    /// for ASCII and Hybrid, where C has no count to clamp.
    ///
    /// Written into `payload`, whose capacity it reuses — C's buffer is the
    /// one `omax` allocation made at init, not one per cycle.
    fn fill_octet_output(&self, payload: &mut Vec<u8>) {
        payload.clear();
        match self.ofmt {
            ASYN_FMT_BINARY => extend_head(payload, &self.bout, self.nowt, self.omax),
            ASYN_FMT_HYBRID => {
                let end = self
                    .bout
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(self.bout.len());
                translate_escape_into(&self.bout[..end], payload);
            }
            _ => translate_escape_into(self.aout.as_bytes(), payload),
        }
        payload.truncate(self.omax.max(0) as usize);
    }

    #[cfg(test)]
    fn octet_output_buffer(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        self.fill_octet_output(&mut payload);
        payload
    }

    /// Snapshot the record fields `performIO` reads into an [`IoPlan`] so the
    /// I/O can run without touching the record.
    #[cfg(test)]
    fn build_io_plan(&self) -> IoPlan {
        let mut plan = IoPlan::default();
        self.fill_io_plan(&mut plan, None);
        plan
    }

    /// The record's trace context into `out`, reusing its buffers.
    fn fill_io_trace(&self, out: &mut IoTrace) {
        let rung = self.trace_rung();
        match (&mut out.port, rung.port) {
            (Some(buf), Some(port)) => {
                buf.clear();
                buf.push_str(port);
            }
            (slot, port) => *slot = port.map(str::to_owned),
        }
        out.addr = rung.addr;
        out.reason = self.resolved_reason as i32;
        out.name.clear();
        out.name.push_str(self.record_name());
        out.trace = Some(rung.trace);
    }

    /// `pasynRec->name`: the name the record was added under.
    fn record_name(&self) -> &str {
        &self.trace_name
    }

    /// The plan of a cycle that may be a GPIB command instead of a transfer,
    /// into `plan`, reusing its output buffer. A GPIB cycle skips the octet
    /// output build and the NRRD/NOWT clamps: C reaches those only inside
    /// `performOctetIO` (asynRecord.c:1503-1546), which `gpibUniversalCmd` /
    /// `gpibAddressedCmd` replace — a UCMD put must not rewrite the record's
    /// transfer sizes.
    fn fill_io_plan(&self, plan: &mut IoPlan, gpib: Option<GpibCycle>) {
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
        // …and neither does a `TMOD = NoI/O` cycle: C's callback skips
        // `performIO` entirely (asynRecord.c:827), so the clamps and the payload
        // build are as unreachable there as they are for a GPIB command.
        let tmod = TransferMode::from_u16(self.tmod as u16);
        let missing = match gpib {
            Some(_) => (!self.port_has(crate::interfaces::InterfaceType::Gpib))
                .then(|| crate::interfaces::InterfaceType::Gpib.asyn_name()),
            None if tmod == TransferMode::NoIo => None,
            None => (!self.has_interface(iface)).then(|| iface.c_asyn_name()),
        };
        let octet = gpib.is_none()
            && tmod != TransferMode::NoIo
            && iface == InterfaceType::Octet
            && missing.is_none();
        let in_cap = in_len.min(i32::MAX as usize) as i32;
        plan.nowt_clamp =
            (octet && self.ofmt == ASYN_FMT_BINARY && self.nowt > self.omax).then_some(self.omax);
        plan.nrrd_clamp = (octet && self.nrrd > in_cap).then_some(in_cap);
        if octet {
            // Binary sends NOWT bytes of BOUT cut at OMAX: the clamped count.
            self.fill_octet_output(&mut plan.octet_out);
        } else {
            plan.octet_out.clear();
        }
        let octet_out_len = plan.octet_out.len();
        let octet_buf_size = if self.nrrd > 0 {
            (self.nrrd as usize).min(in_len)
        } else {
            in_len
        };
        self.fill_io_trace(&mut plan.trace);
        plan.tmod = tmod;
        plan.iface = iface;
        plan.gpib = gpib;
        plan.missing = missing;
        plan.reason = self.resolved_reason;
        plan.addr = self.addr;
        plan.drv_user.clone_from(&self.drv_user);
        plan.timeout = self.io_timeout();
        plan.octet_out_len = octet_out_len;
        plan.ofmt = self.ofmt;
        plan.i32out = self.i32out;
        plan.ui32out = self.ui32out;
        plan.ui32mask = self.ui32mask;
        plan.f64out = self.f64out;
        plan.octet_buf_size = octet_buf_size;
        plan.error_message =
            ErrorMessage::format(format_args!("{}", self.device_user.error_message()));
        plan.in_len = in_len;
        plan.ifmt = self.ifmt;
    }

    /// Decode this cycle's GPIB command — C `asynCallbackProcess`'s
    /// `ucmd != gpibUCMD_None` / `acmd != gpibACMD_None` test
    /// (asynRecord.c:819-827). UCMD wins over ACMD, as in C. The reset back to
    /// `None` is the callback's, [`IoOutcome::entered`]: a cycle the queue
    /// refused leaves the command pending.
    fn gpib_cycle(&self) -> Option<GpibCycle> {
        use crate::interfaces::gpib::{
            GpibAddressedRequest, addressed_request, universal_cmd_byte,
        };
        if self.ucmd != 0 {
            return Some(GpibCycle::Universal(universal_cmd_byte(self.ucmd)));
        }
        if self.acmd != 0 {
            let request = addressed_request(self.acmd, self.addr);
            return Some(match request {
                GpibAddressedRequest::Frame(frame) => GpibCycle::Addressed(frame),
                GpibAddressedRequest::SerialPoll => GpibCycle::SerialPoll,
            });
        }
        None
    }

    /// Apply the results of a `performIO` cycle to the record's input/status
    /// fields. Single owner of the result→field mapping (C `performIO`
    /// stores into AINP/BINP/I32INP/.../NAWT/EOMR/NORD/ERRS), whether the
    /// cycle ran in place or re-entered from the port thread.
    fn apply_io_outcome(&mut self, out: IoOutcome) {
        if let Some(message) = out.error_message {
            self.device_user.set_error_message(message);
        }
        if let Some(v) = out.nowt {
            self.nowt = v;
        }
        if let Some(v) = out.nrrd {
            self.nrrd = v;
        }
        if let Some(v) = out.ucmd {
            self.ucmd = v;
        }
        if let Some(v) = out.acmd {
            self.acmd = v;
        }
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
            self.set_errs(v);
        }
        // The I/O cycle's record alarm (C `recGblSetSevr` in `performIO`).
        // `check_alarms` — invoked by the framework on this same completion
        // cycle (sync `process()` return or async re-entry) — is the sole
        // consumer; it `take()`s and commits it via `rec_gbl_set_sevr`.
        if let Some(a) = out.alarm {
            self.io_alarm = Some(a);
        }
    }

    /// C `process()`'s `queueRequest(pasynUser, asynQueuePriorityLow,
    /// QUEUE_TIMEOUT)` (asynRecord.c:342-353): write the plan into the record's
    /// request and queue it. On a blocking port the record returns with
    /// `pact = TRUE`; on any other the callback has run when `run_cell`
    /// returns, under one gate, and its outcome is applied now. Nothing here
    /// allocates or spawns — the request was made when the port was connected
    /// ([`Self::process_cell_on`]) — and a refusal is C's `reportError(...,
    /// "queueRequest failed")` (:355, alarmed STATE/MINOR at :361).
    fn queue_process(&mut self, handle: &PortHandle, gpib: Option<GpibCycle>) -> ProcessOutcome {
        let job = Arc::clone(&self.process_job);
        // A record outside a database has no completion to re-enter, so its
        // request runs before this returns whatever the port.
        let queued = handle.can_block() && self.async_ctx.is_some();
        let user = {
            let mut request = job.request.lock();
            self.fill_io_plan(&mut request.plan, gpib);
            request.prio = self.callback_priority;
            request.queued = queued;
            io_user(&request.plan).with_priority(crate::port::QueuePriority::Low)
        };
        let cell = self.process_cell_on(handle);
        let sent = match handle.queue_gate(&user) {
            Err(refusal) => Err(AsynError::from(refusal)),
            Ok(()) if queued => cell.queue(&user).map_err(AsynError::from),
            Ok(()) => handle.run_cell(&cell, &user),
        };
        let out = match sent {
            Ok(()) if queued => {
                self.io_pending = true;
                return ProcessOutcome::async_pending();
            }
            Ok(()) => job.outcome.lock().take(),
            Err(e) => {
                let mut out = IoOutcome::default();
                out.note_error(e.message_display());
                out.report_queue_refused(&job.request.lock().plan.trace);
                Some(out)
            }
        };
        if let Some(out) = out {
            self.apply_io_outcome(out);
        }
        ProcessOutcome::complete()
    }

    /// The record's process request on `handle`'s port — made by
    /// [`Self::connect_device`], or here for a record attached some other way.
    fn process_cell_on(&mut self, handle: &PortHandle) -> Arc<RequestCell> {
        match &self.process_cell {
            Some(cell) if handle.owns_cell(cell) => Arc::clone(cell),
            _ => {
                let job: Arc<dyn CellBody> = self.process_job.clone();
                let cell = handle.new_cell(job);
                self.process_cell = Some(Arc::clone(&cell));
                cell
            }
        }
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

    /// The one field asyn serves a non-default precision for.
    ///
    /// `asynRecord.c:994-1004` is the whole rule:
    ///
    /// ```c
    /// static long get_precision(const struct dbAddr * paddr, long *precision)
    /// {
    ///     int fieldIndex = dbGetFieldIndex(paddr);
    ///     *precision = 0;
    ///     if(fieldIndex == asynRecordTMOT) { *precision = 4; return (0); }
    ///     recGblGetPrec(paddr, precision);
    ///     return (0);
    /// }
    /// ```
    ///
    /// Every other field keeps the literal 0, which is what the port already
    /// serves, so TMOT is the sole divergence: `caget -s A.TMOT` printed `1`
    /// where C prints `1.0000`. [`Record::property_support`] above already
    /// declares `precision: true`; nothing supplied it.
    ///
    /// TMOT's early return skips `recGblGetPrec`, but that is not separately
    /// observable — TMOT is `DBF_DOUBLE` and `recGblGetPrec` only clamps a
    /// double precision outside `[0, 15]` (`recGbl.c:135-139`), so 4 would
    /// survive the call unchanged. Only the 4 itself is reproducible.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        match field {
            "TMOT" => Some(FieldMetadataOverride {
                precision: Some(4),
                ..Default::default()
            }),
            _ => None,
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
    /// `process()` completion re-entry and the exception refresh; a
    /// record never built into a database keeps `None` and stays fully
    /// synchronous.
    fn set_async_context(&mut self, name: epics_base_rs::types::RecordName, db: AsyncDbHandle) {
        let target = name.clone();
        self.cancel_scan = Some(Arc::new(move |db| {
            let passive = EpicsValue::Enum(ScanType::Passive.to_u16());
            let held = db.lock_for_put(&target, "SCAN", &passive);
            let _ = db.put_pv_already_locked(held, passive);
        }));
        *self.process_job.reentry.lock() = Some(db.process_callback(&name));
        *self.special_lane.record.lock() = Some((name.clone(), db.clone()));
        self.trace_name = name.to_string();
        self.async_ctx = Some((name, db));
    }

    fn set_process_context(&mut self, ctx: &epics_base_rs::server::record::ProcessContext) {
        // C re-reads `pPvt->pr->prio` on every completion, so a `PRIO` written
        // between cycles moves the next one.
        self.callback_priority = ctx.callback_priority;
    }

    /// The `recGblSetSevr` calls in the `perform*IO` helpers raise a record alarm
    /// severity for every I/O failure: read error -> READ/MAJOR, ASCII/Hybrid
    /// input overflow -> READ/MINOR, register write error -> WRITE/MAJOR, missing
    /// GPIB interface -> COMM/MAJOR (asynRecord.c:1364-1636/1649/1695). The record
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
            "BOUT" => Some(EpicsValue::CharArray(
                read_head(&self.bout, self.nowt, self.omax).into(),
            )),
            "OMAX" => Some(EpicsValue::Long(self.omax)),
            "NOWT" => Some(EpicsValue::Long(self.nowt)),
            "NAWT" => Some(EpicsValue::Long(self.nawt)),
            "OFMT" => Some(EpicsValue::Enum(self.ofmt as u16)),
            "AINP" => Some(EpicsValue::String(self.ainp.clone().into())),
            "TINP" => Some(EpicsValue::String(self.tinp.clone().into())),
            "IEOS" => Some(EpicsValue::String(self.ieos.clone().into())),
            "BINP" => Some(EpicsValue::CharArray(
                read_head(&self.binp, self.nord, self.imax).into(),
            )),
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
            "SPR" => Some(EpicsValue::UChar(self.spr as u8)),
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
            // C serves ERRS as a `DBR_CHAR` array of exactly `ERR_SIZE`
            // elements (`cvt_dbaddr` :956-962, `get_array_info` :976-979) — the
            // whole buffer, NUL-padded, not the message length. A `DBF_STRING`
            // here clipped a 68-character connect diagnostic to 39 and handed
            // the client the wrong DBR type (R18-85).
            "ERRS" => {
                let mut buf = self.errs.as_bytes().to_vec();
                buf.resize(ERR_SIZE, 0);
                Some(EpicsValue::CharArray(buf.into()))
            }
            "AQR" => Some(EpicsValue::UChar(self.aqr as u8)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Helper closures for type coercion
        let to_i32 = |v: &EpicsValue| -> i32 { v.to_f64().unwrap_or(0.0) as i32 };
        let to_u32 = |v: &EpicsValue| -> u32 { v.to_f64().unwrap_or(0.0) as u32 };
        let to_f64 = |v: &EpicsValue| -> f64 { v.to_f64().unwrap_or(0.0) };
        // The framework's put row has already rendered a number put to a
        // string field, at this record's `get_precision` (`asynRecord.c:994`).
        let to_str = |v: &EpicsValue| -> CaResult<String> {
            match v {
                EpicsValue::String(s) => Ok(s.as_str_lossy().into_owned()),
                _ => Err(CaError::TypeMismatch(name.into())),
            }
        };
        let to_bytes = |v: &EpicsValue| -> Vec<u8> {
            match v {
                EpicsValue::CharArray(b) => b.clone().to_vec(),
                EpicsValue::String(s) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            }
        };

        match name {
            "PORT" => {
                self.port = to_str(&value)?;
            }
            "ADDR" => {
                self.addr = to_i32(&value);
            }
            "PCNCT" => {
                self.pcnct = to_i32(&value);
            }
            "DRVINFO" => {
                self.drvinfo = to_str(&value)?;
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
                self.aout = to_str(&value)?;
            }
            "OEOS" => {
                self.oeos = to_str(&value)?;
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
                self.ainp = to_str(&value)?;
            }
            "TINP" => {
                self.tinp = to_str(&value)?;
            }
            "IEOS" => {
                self.ieos = to_str(&value)?;
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
                self.hostinfo = to_str(&value)?;
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
                self.tfil = to_str(&value)?;
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
            // `cvt_dbaddr` marks ERRS `SPC_NOMOD` (asynRecord.c:962); the
            // database's NOMOD gate refuses a client put.
            "ERRS" => self.store_errs(value)?,
            "AQR" => {
                self.aqr = to_i32(&value);
            }
            _ => {
                return Err(CaError::FieldNotFound(name.to_string()));
            }
        }
        Ok(())
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `init_record` sizes the binary buffers before allocating them:
            // `if(pasynRec->omax <= 0) pasynRec->omax = MAX_STRING_SIZE;` and the
            // same for `imax` (asynRecord.c:293-294), MAX_STRING_SIZE being 40.
            // The dbd's `initial("80")` covers a db that says nothing; this
            // covers a db that says `field(IMAX, "0")` — without it that record
            // gets a zero-length buffer, `dbaddr_capacity` advertises 0, and
            // every read is sized 0 (R18-88).
            if self.omax <= 0 {
                self.omax = AINP_SIZE as i32;
            }
            if self.imax <= 0 {
                self.imax = AINP_SIZE as i32;
            }
        }
        if pass == 1 && !self.port.is_empty() {
            // C init_record (asynRecord.c:281-283) ignores the returned status
            // beyond the state it sets; `connectDevice`'s own "Connect error…"
            // text is what stays in ERRS.
            let _ = self.connect_device();
        }
        Ok(())
    }

    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
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
                if self.connect_device().is_err() {
                    // The buffer is not `connectDevice`'s own text: its tail ran
                    // `monitorStatus` over the same slot, and the last of that
                    // function's three manager queries — `isEnabled` — is what
                    // an unbound record reads back here.
                    let errs = Errs::format(format_args!(
                        "connectDevice failed: {}",
                        self.device_user.error_message()
                    ));
                    // C passes `asynSuccess` (asynRecord.c:514): no trace line.
                    self.report_errs(AsynStatus::Success, errs);
                }
            }

            // C sets the trace and leaves the other half of the mask/bit pair to
            // the `monitorStatus` the setter's exception runs (:409-448).
            "TMSK" => {
                self.apply_trace_mask(TraceMask::from_bits_retain(self.tmsk as u32));
            }
            "TB0" | "TB1" | "TB2" | "TB3" | "TB4" | "TB5" => {
                self.apply_trace_mask(self.trace_mask_of_bits());
            }
            "TIOM" => {
                self.apply_trace_io_mask(TraceIoMask::from_bits_retain(self.tiom as u32));
            }
            "TIB0" | "TIB1" | "TIB2" => {
                self.apply_trace_io_mask(self.io_mask_of_bits());
            }
            "TINM" => {
                self.apply_trace_info_mask(TraceInfoMask::from_bits_retain(self.tinm as u32));
            }
            "TINB0" | "TINB1" | "TINB2" | "TINB3" => {
                self.apply_trace_info_mask(self.info_mask_of_bits());
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
                if self.device_user.device().is_some() {
                    self.raise_exception(|rec| {
                        if let Some(entry) = rec.device_user.device() {
                            let _ = entry.handle.set_enable(rec.addr, rec.enbl != 0);
                        }
                    });
                }
            }

            // Auto-connect (C parity: pasynManager->autoConnect from
            // asynRecord.c:481-482). C fires
            // asynExceptionAutoConnect unconditionally, which Rust
            // mirrors via PortDriverBase::set_auto_connect.
            "AUCT" => {
                if self.device_user.device().is_some() {
                    self.raise_exception(|rec| {
                        if let Some(entry) = rec.device_user.device() {
                            let _ = entry.handle.set_auto_connect(rec.addr, rec.auct != 0);
                        }
                    });
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
            // (:1090-1094), so a refused or failed request snaps the field back to
            // the wire's real state — *and posts it*, which is the half that was
            // missing (W10-D2). Without the post the snap-back was invisible: the
            // operator's screen kept showing the value they typed at a port that
            // had refused it.
            "CNCT" => {
                if let Some(entry) = self.queue_special() {
                    let want = self.cnct != 0;
                    let special = self.special_request(
                        &entry.handle,
                        SpecialWhat::Connect { want },
                        OptionQueue::Normal,
                    );
                    self.queue_port_request(entry.handle, special);
                }
            }

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
                    self.device_user.disconnect();
                    self.clear_exception_callback();
                    // Detached: `isConnected` has no device to report on, which
                    // is C's CNCT=0 (monitorStatus, :1093-1094). The refresh goes
                    // in the post bracket like every other one — a field that
                    // moves without its monitor is a field the operator's screen
                    // never learns about (R14-47).
                    // REMEMBER_STATE(cnct), re-read, POST_IF_NEW(cnct).
                    self.old.status[19] = self.cnct;
                    self.refresh_connected_state();
                    if post_if_new(self.cnct, &mut self.old.status[19]) {
                        self.mark(STATUS_AT.map(|at| at + 19));
                    }
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
            // connect readbacks, which is why the whole `monitorStatus` set is
            // what gets POST_IF_NEW'd here and not just DRVINFO.
            "REASON" => {
                self.resolved_reason = self.reason as usize;
                self.drvinfo.clear();
                self.cancel_io_interrupt_scan();
                self.publish_io_intr_binding();
                self.monitor_status();
            }

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
                self.write_option("baud", OptionValue::Choice(val));
            }
            "LBAUD" => {
                // C `sprintf(optionString, "%d", pasynRec->lbaud)` (:1783-1785):
                // the long baud is sent verbatim, not through a menu.
                self.write_option("baud", OptionValue::Lbaud);
            }
            "PRTY" => {
                let val = menu_choice(PARITY_CHOICES, self.prty);
                self.write_option("parity", OptionValue::Choice(val));
            }
            "DBIT" => {
                // C parity: `drvAsynSerialPort.c:146/360` recognises the key
                // `"bits"` (not `"csize"`), and so does this port's serial driver
                // (`serial_port.rs:649`).
                let val = menu_choice(DBIT_CHOICES, self.dbit);
                self.write_option("bits", OptionValue::Choice(val));
            }
            "SBIT" => {
                let val = menu_choice(SBIT_CHOICES, self.sbit);
                self.write_option("stop", OptionValue::Choice(val));
            }
            "MCTL" => {
                let val = menu_choice(MCTL_CHOICES, self.mctl);
                self.write_option("clocal", OptionValue::Choice(val));
            }
            "FCTL" => {
                let val = menu_choice(FCTL_CHOICES, self.fctl);
                self.write_option("crtscts", OptionValue::Choice(val));
            }
            "IXON" => {
                let val = menu_choice(IX_CHOICES, self.ixon);
                self.write_option("ixon", OptionValue::Choice(val));
            }
            "IXOFF" => {
                let val = menu_choice(IX_CHOICES, self.ixoff);
                self.write_option("ixoff", OptionValue::Choice(val));
            }
            "IXANY" => {
                let val = menu_choice(IX_CHOICES, self.ixany);
                self.write_option("ixany", OptionValue::Choice(val));
            }

            // --- IP options ---
            "HOSTINFO" => {
                self.write_option("hostinfo", OptionValue::Hostinfo);
            }
            "DRTO" => {
                let val = menu_choice(DRTO_CHOICES, self.drto);
                self.write_option("disconnectOnReadTimeout", OptionValue::Choice(val));
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
            // `wasQueued==true`, asynManager.c:1663-1668) does it report
            // "I/O request canceled", raise STATE_ALARM/MAJOR_ALARM and force
            // a completion callback. In every case it then sets
            // state = stateIdle.
            //
            // While a blocking port's request is pending (`io_pending`),
            // cancelling the record's `RequestCell` reproduces the `wasQueued`
            // split by construction: `cancel()` wins only while the request is
            // still queued (the port thread has not claimed its token). Then,
            // on this thread, it unlinks the request, `ProcessJob::unrun`
            // records the "I/O request canceled" outcome (CANCELED_MSG) — the
            // `wasQueued==true` analogue — and `done` requests the record's
            // `ProcessCallback`, as C's `special()` does (asynRecord.c:396-404).
            // A cancel that arrives after the port thread claimed it loses the
            // CAS and is a no-op: the I/O completes and applies normally,
            // matching C `cancelRequest` returning `wasQueued==0` once the
            // callback is active (asynManager.c:1647-1661). Either way that
            // callback's re-entry is the single owner that applies the outcome
            // to ERRS and clears `io_pending` (the `state = stateIdle`
            // transition), so AQR must NOT touch `io_pending` here.
            // The C `wasQueued` branch's STATE_ALARM/MAJOR_ALARM (:399) rides on
            // that same outcome: `IoOutcome::report_canceled` owns both halves of
            // the cancel event, so the re-entry commits the alarm with the message.
            //
            // With no request in flight (synchronous port, or already
            // completed) AQR is the C `wasQueued==false` idle no-op.
            "AQR" => {
                if let (true, Some(cell)) = (self.io_pending, &self.process_cell) {
                    cell.cancel();
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

    /// C `process()` (asynRecord.c:324-372). The body is `Self::process_cycle`;
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
            self.monitor();
        }
        outcome
    }

    /// C `getIoIntInfo` (asynRecord.c:582-597), reached from `dbScan`'s
    /// `scanAdd` / `scanDelete`: register the driver interrupt callbacks when
    /// the record joins the I/O Intr scan list, cancel them when it leaves.
    ///
    /// `registerInterrupts` failing is C's `return -1`, on which `scanAdd`
    /// leaves the record Passive (dbScan.c:286-288).
    fn set_io_intr_scan(&mut self, active: bool) -> bool {
        self.io_intr
            .set_active(active)
            .map_err(|refusal| self.report_register_refusal(refusal))
            .is_ok()
    }

    /// The same `getIoIntInfo(0)` from iocInit's `scanAdd`, for a record loaded
    /// with SCAN="I/O Intr".
    fn io_intr_scan_add(&mut self) -> bool {
        self.io_intr
            .scan_add()
            .map_err(|refusal| self.report_register_refusal(refusal))
            .is_ok()
    }

    fn take_after_gate(&mut self) -> Option<epics_base_rs::server::record::AfterGate> {
        if !std::mem::take(&mut self.cancel_scan_owed) {
            return None;
        }
        self.cancel_scan
            .as_ref()
            .map(epics_base_rs::server::record::AfterGate::shared)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// asynRecord.c's one `udf` statement is init_record pass 0's `udf = 0`
    /// (:303), and it has no `checkAlarms`: process neither clears a UDF put
    /// back to 1 nor alarms on it.
    fn clears_udf(&self) -> bool {
        false
    }

    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// C `asynRecord.c` init_record pass 0: `pasynRec->udf = 0;` (`:303`) —
    /// set unconditionally, before any connect, so a freshly loaded asyn
    /// record is defined even against a disconnected port.
    fn post_init_finalize_undef(&mut self, udf: &mut u8) -> CaResult<()> {
        *udf = 0;
        Ok(())
    }

    /// C `asynRecord.c:304`: `recGblResetAlarms(pasynRec)`, so the record is
    /// no-alarm at load. The init owner performs the reset — see
    /// [`Record::init_resets_alarms`].
    fn init_resets_alarms(&self) -> bool {
        true
    }

    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::CYCLE_POSTED
            .union(CycleHooks::CONTEXT)
            .union(CycleHooks::PROCESS_POSTED)
            .union(CycleHooks::WRITE_POSTED)
    }

    /// `monitor()`'s marks at the end of a cycle, and on the put path the
    /// marks `special()` made, drained after it returns and before dbPut's
    /// post (`dbAccess.c:1398` vs `:1408`).
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        self.post_cursor = 0;
        CycleMarks::of(&ASYN_POSTS, std::mem::take(&mut self.post_marks))
    }

    /// The marks `process()` made itself, drained as it returns: ahead of the
    /// cycle's alarm commit and stamp, as C's posts from inside `process()`.
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        let held = self.post_marks & HELD_MARKS;
        self.post_marks &= !HELD_MARKS;
        self.post_cursor = 0;
        CycleMarks::of(&ASYN_POSTS, held)
    }

    /// A process cycle posts only what `monitor()` names: ERRS and the
    /// `monitorStatus` set post from their marks (`mark`), and VAL
    /// never posts.
    fn process_posted_fields(&self) -> Option<epics_base_rs::server::record::DeclaredFields> {
        static CLAIMED: [epics_base_rs::server::record::FieldIx; 14] =
            epics_base_rs::server::record::declared_set(
                epics_base_rs::server::record::dbd_generated::ASYN_FIELDS,
                MONITOR_CLAIMED,
            );
        Some(&CLAIMED)
    }

    /// `monitor()` decides these from its own old copies, never from a
    /// published one: a put posts NRRD itself, and C's next cycle re-posts it
    /// when `REMEMBER_STATE` did not run.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &MONITOR_CLAIMED
    }

    /// The `SPC_DBADDR` octet buffers: C `cvt_dbaddr` (asynRecord.c:948-955)
    /// fixes the channel's `no_elements = omax` (BOUT) / `imax` (BINP) — the
    /// buffer capacity, `80` by default — while `get_array_info` reports the
    /// current transferred length (`nowt`/`nord`). So `ca_element_count` is the
    /// capacity even though `get_field` serves the shorter transferred bytes.
    fn dbaddr_capacity(&self, field: &str) -> Option<u32> {
        match field {
            "BOUT" => Some(self.omax.max(0) as u32),
            "BINP" => Some(self.imax.max(0) as u32),
            // ERRS is the third `SPC_DBADDR` field, and its capacity is the
            // fixed `ERR_SIZE` (asynRecord.c:958).
            "ERRS" => Some(ERR_SIZE as u32),
            _ => None,
        }
    }

    fn dbaddr_capacity_inputs(&self) -> Option<&'static [&'static str]> {
        Some(&[])
    }

    /// `post_fields_with_mask` writes a posted ERRS back through this path;
    /// it goes straight to `store_errs`, since the default's coercion
    /// would allocate the `ERR_SIZE` buffer `get_field` serves.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "ERRS" {
            return self.store_errs(value);
        }
        epics_base_rs::server::record::put_field_internal_default(self, name, value)
    }
}

impl AsynRecord {
    /// C `monitor()` (asynRecord.c:1006-1031), run from `done:`: the input
    /// buffer the IFMT selects (`:1014-1017`) and TINP on every reading cycle,
    /// unconditionally, then the `POST_IF_NEW` block against `old`.
    fn monitor(&mut self) {
        let mut bits = 0u128;
        if matches!(
            TransferMode::from_u16(self.tmod as u16),
            TransferMode::Read | TransferMode::WriteRead
        ) {
            bits |= if self.ifmt == ASYN_FMT_ASCII {
                0b101
            } else {
                0b110
            };
        }
        let old = &mut self.old;
        let moved = [
            post_if_new(self.nrrd, &mut old.nrrd),
            post_if_new(self.nord, &mut old.nord),
            post_if_new(self.nowt, &mut old.nowt),
            post_if_new(self.nawt, &mut old.nawt),
            post_if_new(self.spr, &mut old.spr),
            post_if_new(self.ucmd, &mut old.ucmd),
            post_if_new(self.acmd, &mut old.acmd),
            post_if_new(self.eomr, &mut old.eomr),
            post_if_new(self.i32inp, &mut old.i32inp),
            post_if_new(self.ui32inp, &mut old.ui32inp),
            post_if_new(self.f64inp, &mut old.f64inp),
        ];
        for (bit, moved) in moved.into_iter().enumerate() {
            bits |= (moved as u128) << (bit + 3);
        }
        self.post_marks |= bits << HELD_POSTS;
    }

    /// The body of C's `process()` — every arm of its `state` chain. The
    /// `done:` epilogue that closes the interrupt-cell invariant lives in the
    /// [`Record::process`] wrapper that calls this.
    fn process_cycle(&mut self) -> CaResult<ProcessOutcome> {
        // Completion re-entry of a blocking-port cycle: the port thread left
        // the outcome in the process request and fired the async-record
        // token, re-entering here. This is C's `process()` `pact==TRUE`
        // branch (asynRecord.c:362-363) — apply the I/O results the port
        // thread produced and finish (`state = stateIdle`). Checked first so
        // a completion never re-issues I/O.
        if std::mem::take(&mut self.io_pending) {
            let outcome = self.process_job.outcome.lock().take();
            self.apply_io_outcome(outcome.unwrap_or_default());
            return Ok(ProcessOutcome::complete());
        }

        // Re-run the C `monitorStatus` refresh if any exception fired on the
        // port since the last cycle — an external `setTrace*`, a dropped link,
        // an enable / auto-connect change. C refreshes these readback fields
        // from its `exceptCallback` immediately (asynRecord.c:903-917). When the
        // record carries a database handle the subscription
        // (`register_exception_callback`) does the same under the record's
        // hold and never sets this flag. This dirty path is the
        // fallback for a record with no handle / runtime, draining through the
        // single `monitor_status` owner.
        if self.status_dirty.swap(false, Ordering::AcqRel) {
            self.monitor_status();
        }

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
        let Some(handle) = self.device_user.device().map(|e| e.handle.clone()) else {
            self.report_not_connected();
            return Ok(ProcessOutcome::complete());
        };
        // C `REMEMBER_STATE` (asynRecord.c:335-338), `stateIdle` only: a put to
        // one of these between cycles posts from its put, not again from
        // `monitor()`. A portless record skips it, so C re-posts such a put.
        let old = &mut self.old;
        (old.ucmd, old.acmd, old.nowt, old.nrrd) = (self.ucmd, self.acmd, self.nowt, self.nrrd);

        // Every idle process clears ERRS before it decides what to do: C
        // `process()` calls `resetError` in its `stateIdle` arm (asynRecord.c:339)
        // and the callback it queues calls it again on entry (:817) — both
        // *above* the UCMD / ACMD / TMOD dispatch. So a record that failed a
        // transfer and is then scanned with TMOD=NoIO comes back with an empty
        // ERRS. This port merges C's process and its callback, so one reset here
        // is both of C's. The `stateNoDevice` refusal above does not reset: its
        // unchanged text posts nothing.
        self.reset_error();

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
        // GPIB command, else the TMOD/IFACE transfer (performIO). The interface
        // refusals ("No asynInt32 interface", "No asynGpib interface") are that
        // callback's too, so they wait for the queue gate like every transfer:
        // a disabled port answers "queueRequest failed" whatever IFACE says.
        let gpib = self.gpib_cycle();
        Ok(self.queue_process(&handle, gpib))
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

    /// A take's marks by declared name.
    fn named(marks: CycleMarks) -> Vec<(&'static str, CyclePostMask)> {
        marks
            .map(|(ix, mask)| {
                let desc = epics_base_rs::server::record::field_at(
                    epics_base_rs::server::record::dbd_generated::ASYN_FIELDS,
                    ix,
                );
                (desc.expect("declared").name, mask)
            })
            .collect()
    }
    use epics_base_rs::server::record::RecordProcessResult;

    /// `asynRecord.c:994-1004`: `*precision = 0` for every field, then a
    /// single `fieldIndex == asynRecordTMOT` arm that returns 4. Both halves
    /// matter — TMOT is the ONLY asyn field with a non-default precision, so a
    /// blanket precision arm would be as wrong as none at all. With TMOT at its
    /// default 1.0, C's `caget -s A.TMOT` prints `1.0000`.
    #[test]
    fn only_tmot_carries_a_non_default_precision() {
        let rec = AsynRecord::default();

        let prec = |f: &str| rec.field_metadata_override(f).and_then(|o| o.precision);
        assert_eq!(prec("TMOT"), Some(4), "asynRecordTMOT arm");

        // The other two DBF_DOUBLE fields fall to C's literal 0, as does every
        // non-double field; `recGblGetPrec` leaves an in-range double alone.
        for field in ["F64INP", "F64OUT", "VAL", "AOUT", "AINP", "NRRD", "ADDR"] {
            assert!(
                rec.field_metadata_override(field).is_none(),
                "{field} must not override precision"
            );
        }

        // The record declares precision as supplied; that declaration is only
        // honest because the arm above exists.
        assert!(rec.property_support().precision);
    }

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
        // 76 served fields plus the two `DBF_NOACCESS` pointers `OPTR` and
        // `IPTR`, carried for `dbpr` like every pointer member.
        assert_eq!(rec.field_list().len(), 78);
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

    /// R18-85. ERRS is not a `DBF_STRING`. C `cvt_dbaddr` gives it
    /// `pfield = pasynRec->errs`, `no_elements = ERR_SIZE`,
    /// `field_type = DBF_CHAR`, `dbr_field_type = DBR_CHAR` and
    /// `special = SPC_NOMOD` (asynRecord.c:956-962), and `get_array_info`
    /// reports `ERR_SIZE` again (`:976-979`). Serving it as a 40-byte string
    /// clipped a 68-character connect diagnostic at 39 characters and handed
    /// the client `DBR_STRING` for a field C declares `DBR_CHAR`.
    #[test]
    fn errs_is_a_hundred_byte_char_array() {
        let mut rec = AsynRecord::default();

        // The connect-error shape the row names.
        let long = "Connect error, status=3, port sim_serial_port_1 is not connected to a d";
        assert_eq!(long.len(), 71, "well past a DBF_STRING's 39");
        rec.report_error(AsynStatus::Success, format_args!("{long}"));

        let Some(EpicsValue::CharArray(buf)) = rec.get_field("ERRS") else {
            panic!("ERRS must serve as a char array, not a string");
        };
        assert_eq!(buf.len(), ERR_SIZE, "C reports no_elements = ERR_SIZE");
        assert_eq!(&buf[..long.len()], long.as_bytes(), "no 39-byte clip");
        assert!(buf[long.len()..].iter().all(|&b| b == 0), "NUL-padded");
        assert_eq!(rec.dbaddr_capacity("ERRS"), Some(ERR_SIZE as u32));

        // `epicsVsnprintf(buffer, ERR_SIZE, ...)` (:2037) is the real bound:
        // at most ERR_SIZE - 1 characters reach the field.
        rec.report_error(AsynStatus::Success, format_args!("{}", "x".repeat(250)));
        assert_eq!(rec.errs.len(), ERR_SIZE - 1);
        // A char that does not fit ends the text: nothing after it lands.
        rec.report_error(
            AsynStatus::Success,
            format_args!("{}{}{}", "y".repeat(ERR_SIZE - 2), 'é', '!'),
        );
        assert_eq!(rec.errs, "y".repeat(ERR_SIZE - 2).as_str());
        rec.report_error(AsynStatus::Success, format_args!("{}", "x".repeat(250)));

        // put_field stores ERRS; the database's NOMOD gate refuses a client.
        rec.put_field("ERRS", EpicsValue::String("operator text".into()))
            .unwrap();
        assert_eq!(rec.errs, "operator text");

        // The record's own post owner writes through the internal path.
        rec.put_field_internal("ERRS", errs_on_the_wire("Read error, timeout"))
            .unwrap();
        assert_eq!(rec.errs, "Read error, timeout");
    }

    /// The attach's own readbacks report the gate that refused them.
    ///
    /// C `connectDevice` queues two readbacks and reports each refusal itself:
    /// `callbackGetOption` carries `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` at
    /// connect priority and so is *not* refused on a down port, while
    /// `callbackGetEos` goes at `asynQueuePriorityLow` carrying no waiver and so
    /// is — `reportError("queueRequest failed\n")` (asynRecord.c:1296-1301).
    ///
    /// Measured on the fat `softIoc` with
    /// `drvAsynIPPortConfigure("ORACLEASYN","localhost:1",0,1,0)` and one
    /// `record(asyn,"T:ASYN"){field(PORT,"ORACLEASYN")}`: at boot, before any
    /// process, `T:ASYN.ERRS` reads `queueRequest failed` + byte 10, IEOS/OEOS are
    /// blank and HOSTINFO is `localhost:1`. The port answered the same on every
    /// field *except* ERRS, which stayed 100 NULs: `query_options` and
    /// `read_eos_from_driver` classified only the queue *timeout* and let the
    /// gate's refusal fall through to their "the driver does not know this" arm,
    /// so the one message C puts on an operator's screen on every attach to a
    /// disconnected port was never written.
    ///
    /// One case per boundary of [`AsynRecord::report_never_ran`]: the readback the
    /// waiver carries past the gate (silent), the readback the gate refuses
    /// (reported), and a driver-level key failure inside a request that *did* run
    /// (silent — C ignores `getOption`'s status, asynRecord.c:1864-1927).
    #[test]
    fn a_refused_attach_readback_reports_the_gate() {
        use crate::drivers::null_port::NullOctetPort;
        use crate::interrupt::InterruptManager;
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        let port_name = "errs_attach_readback_refused";
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(NullOctetPort::new(port_name)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.init_record(1).unwrap();

        assert_eq!(
            rec.pcnct, 1,
            "precondition: the record attached to the port"
        );
        assert_eq!(rec.cnct, 0, "precondition: the link is down");
        assert_eq!(rec.octetiv, 1, "precondition: the EOS readback is reached");
        assert_eq!(
            rec.ieos, "",
            "precondition: the EOS readback never reached the driver"
        );
        assert_eq!(rec.oeos, "");

        assert_eq!(
            rec.errs, CONNECT_READBACK_QUEUE_REFUSED_MSG,
            "connectDevice reports the readback the queue gate refused"
        );
        // …and it reaches a client as C's bytes, newline included.
        let Some(EpicsValue::CharArray(buf)) = rec.get_field("ERRS") else {
            panic!("ERRS serves as a char array");
        };
        assert_eq!(&buf[..20], b"queueRequest failed\n");
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
        let expected = TraceMask::ERROR | TraceMask::IO_DEVICE | TraceMask::WARNING;
        assert_eq!(rec.trace_mask_of_bits(), expected);
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
        assert!(rec.device_user.device().is_none());
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
    /// branch (asynRecord.c:397-404) — "I/O request canceled" **and**
    /// `recGblSetSevr(pasynRec, STATE_ALARM, MAJOR_ALARM)`. The message and the
    /// severity are one event, so the outcome that carries the text to the
    /// completion re-entry carries the alarm with it.
    #[test]
    fn a_canceled_queued_request_raises_state_major() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;

        // C `cancelRequest` with the request still queued: `wasQueued == true`.
        rec.process_job.unrun(Unrun::Canceled);
        let out = rec.process_job.outcome.lock().take().unwrap();
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
    /// "Connect error, status=%d, %s" (asynRecord.c:1160-1161) — where `%s` is the
    /// manager's `pasynUser->errorMessage` (asynManager.c:1331,1339) — and,
    /// on the `special()` PORT/ADDR/DRVINFO path only, the wrapper
    /// "connectDevice failed: %s" (asynRecord.c:515).
    ///
    /// The two `%s` are the *same buffer* read at two moments, and they do not
    /// read the same thing. `connectDevice` reports at the point of failure,
    /// while the manager's connect text is still there; `special()` reports
    /// after the `bad:` → `done:` tail, and that tail's `monitorStatus` has
    /// meanwhile queried `isAutoConnect`, `isConnected` and `isEnabled` on a
    /// user the same function unbound at :1154 — three failures, each
    /// overwriting the buffer (asynManager.c:2331-2373). So the wrapper names
    /// `isEnabled`, the last of the three, and never the port. Measured on the
    /// C IOC: a PORT put naming a port that does not exist reads back
    /// "connectDevice failed: asynManager:isEnabled asynUser not connected to
    /// device".
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

        // special() PORT path: the wrapper overwrites it — with the text the
        // tail's last query left in the buffer, not with the connect failure.
        let mut rec = AsynRecord::default();
        rec.port = "NO_SUCH_PORT_R9_51".to_string();
        rec.special("PORT", true, &Default::default()).unwrap();
        assert_eq!(
            rec.errs,
            "connectDevice failed: asynManager:isEnabled asynUser not connected to device"
        );

        // An empty PORT takes the manager's other rejection — and lands on the
        // same wrapper text, because the tail that overwrites the buffer is the
        // one both failures fall into.
        let mut rec = AsynRecord::default();
        rec.special("PORT", true, &Default::default()).unwrap();
        assert_eq!(
            rec.errs,
            "connectDevice failed: asynManager:isEnabled asynUser not connected to device"
        );
        assert_eq!(
            rec.device_user.error_message(),
            "asynManager:isEnabled asynUser not connected to device",
            "the record's user keeps the last diagnostic; nothing clears it"
        );
    }

    /// asynRecord.c:651-653 prints `pasynUser->errorMessage` after any failed
    /// `registerInterrupts`. getInterruptPvt writes it; "No asynInt32
    /// interface" goes to ERRS only, so that line repeats what the last
    /// manager or driver call left there.
    #[test]
    fn a_register_refusal_prints_the_users_last_error_message() {
        use super::io_intr::NoInterface;
        let mut rec = AsynRecord::default();
        rec.trace_name = "R".into();
        rec.port = "NO_SUCH_PORT_RI".to_string();
        rec.init_record(1).unwrap();
        let no_iface = || RegisterRefusal::NoInterface(NoInterface("asynInt32"));

        assert_eq!(
            rec.register_refusal_line(no_iface()),
            "R registerInterrupts asynManager:isEnabled asynUser not connected to device"
        );
        assert_eq!(rec.errs, "No asynInt32 interface");

        let pvt =
            "asynManager:getInterruptPvt Driver does not support interrupts on interface asynInt32";
        let refused = RegisterRefusal::Refused(AsynError::Status {
            status: AsynStatus::Error,
            message: pvt.into(),
        });
        assert_eq!(
            rec.register_refusal_line(refused),
            format!("R registerInterrupts {pvt}")
        );
        assert_eq!(
            rec.register_refusal_line(no_iface()),
            format!("R registerInterrupts {pvt}")
        );

        let mut out = IoOutcome::default();
        out.note_error("driver read failed");
        rec.apply_io_outcome(out);
        assert_eq!(
            rec.register_refusal_line(no_iface()),
            "R registerInterrupts driver read failed"
        );
    }

    /// C `connectDevice` binds to a shut-down port — `locatePort` still finds it
    /// — and the asynCommon lookup after it refuses (asynRecord.c:1163-1171,
    /// asynManager.c:1487-1491): "findInterface common: %s", then the `bad:`
    /// tail. PCNCT keeps that text; PORT wraps the `isEnabled` text the tail
    /// left, as it does for every connect failure.
    #[test]
    fn a_shut_down_port_fails_the_asyn_common_lookup() {
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct Port(PortDriverBase);
        impl PortDriver for Port {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let name = "find_common_shut_down";
        let flags = PortFlags {
            destructible: true,
            ..PortFlags::default()
        };
        let (rt, _jh) = create_port_runtime_unclaimed(
            Port(PortDriverBase::new(
                name,
                1,
                flags,
                InterfaceMask::NONE,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();
        rt.port_handle()
            .submit_blocking(crate::request::RequestOp::ShutdownPort, AsynUser::default())
            .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = name.to_string();
        rec.pcnct = 1;
        rec.special("PCNCT", true, &Default::default()).unwrap();
        assert_eq!(
            rec.errs,
            "findInterface common: asynManager:findInterface: port shut down"
        );
        assert_eq!(rec.pcnct, 0);
        assert!(rec.device_user.device().is_none(), "the bad: tail unbinds");

        rec.special("PORT", true, &Default::default()).unwrap();
        assert_eq!(
            rec.errs,
            "connectDevice failed: asynManager:isEnabled asynUser not connected to device"
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

    /// R18-88. C `init_record` sizes the binary buffers before allocating them:
    /// `if(pasynRec->omax <= 0) pasynRec->omax = MAX_STRING_SIZE;` and the same
    /// for `imax` (asynRecord.c:293-294). The dbd's `initial("80")` covers a db
    /// that says nothing about them; the fixup covers a db that says `0` (or a
    /// negative) outright, which otherwise leaves a zero-length buffer, a
    /// channel advertising 0 elements, and reads sized 0.
    #[test]
    fn init_record_floors_a_zero_omax_imax_at_forty() {
        use epics_base_rs::server::record::Record;

        let mut rec = AsynRecord::default();
        rec.omax = 0;
        rec.imax = -1;
        rec.init_record(0).unwrap();
        assert_eq!(rec.omax, 40, "C MAX_STRING_SIZE, not the dbd's 80");
        assert_eq!(rec.imax, 40);
        assert_eq!(rec.dbaddr_capacity("BOUT"), Some(40));
        assert_eq!(rec.dbaddr_capacity("BINP"), Some(40));

        // A db that sized them itself is left alone — the fixup is a floor at
        // zero, not a default.
        let mut sized = AsynRecord::default();
        sized.omax = 256;
        sized.imax = 512;
        sized.init_record(0).unwrap();
        assert_eq!((sized.omax, sized.imax), (256, 512));
    }

    /// R18-87. C's `process()` queues without consulting TMOD (asynRecord.c:342-353
    /// — its only skip is the `gotValue` I/O Intr readback at `:341`) and
    /// only the callback body skips the transfer (`:827`). So a `TMOD = NoI/O`
    /// cycle is a real queue entry, and everything that follows from one
    /// follows here: the port-thread wake that runs auto-connect — the
    /// reconnect-nudge idiom, a scan whose only job is to poke a down link — and
    /// the queue gate, which is what a refusing port answers with.
    ///
    /// The gate outcome is what this pins, because it is the mechanism and it is
    /// deterministic: the auto-connect attempt riding on the same wake is
    /// suppressed by C's own throttle window (`asynManager.c:714-715`) for two
    /// seconds after the attach every record does at `connect_device`.
    ///
    /// Boundaries: a gate that admits (connected port) and a gate that refuses
    /// (disconnected, auto-connect off). Before the fix the cycle returned above
    /// the queue submit, so neither reached the port and both looked identical
    /// and healthy.
    #[test]
    fn a_noio_process_enters_the_port_queue() {
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct Link(PortDriverBase);
        impl PortDriver for Link {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let attach = |name: &'static str, connected: bool| {
            let mut base = PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION
                    | InterfaceMask::INT32
                    | InterfaceMask::UINT32_DIGITAL
                    | InterfaceMask::FLOAT64
                    | InterfaceMask::OCTET
                    | InterfaceMask::DRV_USER,
                InterfaceMask::NONE,
            );
            base.init_connected(connected);
            base.init_auto_connect(false);
            let (rt, _jh) = create_port_runtime_unclaimed(Link(base), RuntimeConfig::default())
                .expect("the port runtime thread must start");
            register_port(
                name,
                rt.port_handle().clone(),
                &crate::registry::open_owner(),
            )
            .unwrap();
            let mut rec = AsynRecord::default();
            rec.port = name.to_string();
            rec.connect_device().unwrap();
            rec.tmod = TransferMode::NoIo as i32;
            (rec, rt)
        };

        // The gate admits: the request ran, and an empty body reports nothing.
        let (mut up, _rt_up) = attach("r18_87_up", true);
        up.process().unwrap();
        assert_eq!(up.errs, "", "an admitted NoI/O cycle reports nothing");
        assert_eq!(
            read_alarm(&mut up),
            (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm)
        );

        // The gate refuses: C turns `queueRequest`'s failure into
        // "queueRequest failed" + STATE/MINOR (asynRecord.c:355-361).
        let (mut down, _rt_down) = attach("r18_87_down", false);
        down.process().unwrap();
        assert_eq!(down.errs, PROCESS_QUEUE_REFUSED_MSG);
        assert_eq!(
            read_alarm(&mut down),
            (alarm_status::STATE_ALARM, AlarmSeverity::Minor),
        );
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
        rec.errs = Errs::from("Read error, timeout");

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
        rec.errs = Errs::from("Write error, nout=0, timeout");
        rec.tmsk = 0;

        rec.special("TMSK", true, &Default::default()).unwrap();

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
        rec.errs = Errs::from("Read error, timeout");

        rec.special("VAL", true, &Default::default()).unwrap();
        rec.special("ERRS", true, &Default::default()).unwrap();

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
        use crate::port::InterfaceMask;
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
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET | InterfaceMask::DRV_USER,
            InterfaceMask::NONE,
        );
        // The port's first parameter — reason 0.
        assert_eq!(
            base.params.create_param("FIRST", ParamType::Int32).unwrap(),
            0
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(ParamDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "FIRST".to_string();
        rec.errs = Errs::from("Connect error, status=3, port down");

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
        use crate::port::InterfaceMask;
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
                // A real driver folds at the entry (`epicsStrCaseCmp`,
                // drvAsynIPPort.c:953); a fixture that matched the caller's
                // spelling would answer only the record's current literal and
                // hide a regression in what spelling the record forwards.
                let key = option_key(key);
                self.log
                    .lock()
                    .unwrap()
                    .option_sets
                    .push((key.clone(), value.to_string()));
                if key == "hostinfo" {
                    *self.host.lock().unwrap() = value.to_string();
                }
                Ok(())
            }
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
                let key = option_key(key);
                match key.as_str() {
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
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION,
            InterfaceMask::NONE,
        );
        // The port is down and nothing will bring it back: C's `autoConnectDevice`
        // is not consulted for a waived request, and must not be needed for one.
        base.init_auto_connect(false);
        base.set_connected(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(DeadLineDriver {
                base,
                log: log.clone(),
                host: host.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        rec.special("HOSTINFO", true, &Default::default()).unwrap();
        assert_eq!(
            log.lock().unwrap().option_sets,
            vec![("hostinfo".to_string(), "newhost:5000".to_string())],
            "the only route to repoint a dead IP port must not be refused"
        );
        assert_eq!(rec.errs, "", "a waived put reports no error");
        assert_eq!(rec.hostinfo, "newhost:5000");

        // An ordinary option put carries no waiver: C queues it at Low priority
        // (asynRecord.c:561-565) and the gate refuses it.
        log.lock().unwrap().option_sets.clear();
        rec.baud = baud_choice_index("19200");
        rec.special("BAUD", true, &Default::default()).unwrap();
        assert!(
            log.lock().unwrap().option_sets.is_empty(),
            "a BAUD put on a disconnected port is asynDisconnected in C, not a driver call"
        );
        // The refusal is `queueRequest`'s return value, so C reports
        // `pasynUserSpecial->errorMessage` (asynRecord.c:571-578) — *not* the
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
        rec.special("IEOS", true, &Default::default()).unwrap();
        assert!(
            log.lock().unwrap().eos_sets.is_empty(),
            "the EOS put must not inherit the HOSTINFO waiver"
        );
    }

    /// The waiver above must survive the key's spelling, the way
    /// `get_option_folds_the_key_case_like_set_option` makes the serial driver's
    /// dispatch survive it. C settles the spelling in two places and they do not
    /// agree with a lowercase literal: `asynRecord` writes the key capital-I
    /// (`"hostInfo"`, asynRecord.c:1825) and both driver halves match it with
    /// `epicsStrCaseCmp` (drvAsynIPPort.c:953, :991), which is a fold, not a
    /// comparison.
    ///
    /// [`OptionQueue::for_key`] is the single owner of "may this option request
    /// run on a disconnected port", so a spelling it fails to recognise does not
    /// merely mis-route: it hands the put the `Normal` queue, the gate refuses it
    /// on the down port, and the operator loses the only route to repoint a port
    /// aimed at a wrong host. No caller forwards such a spelling today — the
    /// twelve `write_option` sites pass literals, and the HOSTINFO one passes
    /// `"hostinfo"` — but `"disconnectOnReadTimeout"` at the DRTO arm shows the
    /// dispatch already emits camelCase, so the distance to the defect is one
    /// literal.
    #[test]
    fn the_hostinfo_queue_waiver_survives_the_keys_spelling() {
        for spelling in ["hostinfo", "hostInfo", "HOSTINFO", "HostInfo", " hostInfo "] {
            assert_eq!(
                OptionQueue::for_key(spelling),
                OptionQueue::EvenIfNotConnected,
                "{spelling} must take C's disconnected-port waiver"
            );
        }
        for spelling in ["baud", "BAUD", "disconnectOnReadTimeout", "hostinfos", ""] {
            assert_eq!(
                OptionQueue::for_key(spelling),
                OptionQueue::Normal,
                "{spelling} must stay on the ordinary queue"
            );
        }
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
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        /// Logs every `setOption` it is given and refuses the ones it cannot
        /// parse — a real serial driver's answer to "Unknown"
        /// (drvAsynSerialPort.c:594-598 reports `Unsupported key "%s"`).
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
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(LoggingDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                sets: sets.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        sets.lock().unwrap().clear();

        // The operator puts BAUD = Unknown (menu index 0) on a port running at
        // 9600, with PRTY already read back as "even".
        assert_eq!(rec.baud, baud_choice_index("9600"));
        assert_eq!(rec.prty, 2);
        rec.baud = 0;
        rec.special("BAUD", true, &Default::default()).unwrap();

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
        rec.special("HOSTINFO", true, &Default::default()).unwrap();
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
        use crate::port::InterfaceMask;
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TfilDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.tfil = "/nonexistent-dir-r9-51/trace.log".to_string();
        rec.special("TFIL", true, &Default::default()).unwrap();
        assert_eq!(
            rec.errs,
            "Error opening trace file: /nonexistent-dir-r9-51/trace.log"
        );
    }

    use crate::port::InterfaceMask;
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
                base: PortDriverBase::new(
                    name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::INT32 | InterfaceMask::OCTET | InterfaceMask::GPIB,
                    InterfaceMask::NONE,
                ),
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
            crate::interfaces::gpib::GPIB_PORT_INTERFACES.capabilities()
        }
        fn gpib_universal_cmd(&mut self, _user: &mut AsynUser, cmd: u8) -> AsynResult<()> {
            self.calls.lock().unwrap().universal.push(cmd);
            self.check()
        }
        fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
            self.calls.lock().unwrap().addressed.push(data.to_vec());
            self.check()
        }
        fn read_octet(
            &mut self,
            _user: &AsynUser,
            buf: &mut [u8],
        ) -> crate::interpose::OctetReadResult {
            self.calls.lock().unwrap().reads += 1;
            match self.poll_byte {
                Some(b) if !buf.is_empty() => {
                    buf[0] = b;
                    crate::interpose::OctetReadResult::ok(1, crate::interpose::EomReason::empty())
                }
                _ => crate::interpose::OctetReadResult::ok(0, crate::interpose::EomReason::empty()),
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
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        let (rt, _jh) = create_port_runtime_unclaimed(port, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
    /// (asynRecord.c:1698-1716, :1748-1749).
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
    /// (:821, :825).
    #[test]
    fn a_port_without_asyngpib_refuses_ucmd_and_acmd() {
        use crate::interfaces::octet_transport_capabilities;
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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
        let (rt, _jh) = create_port_runtime_unclaimed(
            OctetTransport(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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

    /// A trace put moves only the field the client wrote: the other half of
    /// the mask/bit pair is `monitorStatus`'s, which runs only from an
    /// exception, and a portless user raises none (asynManager.c:2797-2800).
    #[test]
    fn a_portless_trace_put_leaves_the_other_half_alone() {
        let mut rec = AsynRecord::default();
        let bits = [rec.tb0, rec.tb1, rec.tb2, rec.tb3, rec.tb4, rec.tb5];
        rec.tmsk = (TraceMask::ERROR | TraceMask::WARNING | TraceMask::FLOW).bits() as i32;
        rec.special("TMSK", true, &Default::default()).unwrap();
        assert_eq!([rec.tb0, rec.tb1, rec.tb2, rec.tb3, rec.tb4, rec.tb5], bits);

        let tmsk = rec.tmsk;
        rec.tb3 = 1;
        rec.special("TB3", true, &Default::default()).unwrap();
        assert_eq!(rec.tmsk, tmsk);
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
    /// `post_init_finalize_undef` and [`Record::init_resets_alarms`] are
    /// honoured) actually runs — the record
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

    /// C `process()` never writes `udf` — only init_record pass 0 does
    /// (asynRecord.c:303) — so `caput A.UDF 1` processes and reads back 1.
    #[tokio::test]
    async fn process_leaves_udf_as_put() {
        use epics_base_rs::server::ioc_builder::IocBuilder;
        use std::collections::HashMap;

        let macros = HashMap::new();
        let (name, factory) = crate::asyn_record::asyn_record_factory();
        let (db, _autosave) = IocBuilder::new()
            .register_record_type(name, factory)
            .db_string("record(asyn, \"TEST:ASYN:UDFPUT\") {}\n", &macros)
            .unwrap()
            .build()
            .await
            .unwrap();

        db.put_record_field_from_ca("TEST:ASYN:UDFPUT", "UDF", EpicsValue::UChar(1))
            .await
            .unwrap();
        let rec = db.get_record("TEST:ASYN:UDFPUT").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.get_common_field("STAT"),
            Some(EpicsValue::Short(alarm_status::STATE_ALARM as i16)),
            "the put processed: no port is STATE_ALARM (asynRecord.c:356-361)"
        );
        assert_eq!(inst.get_common_field("UDF"), Some(EpicsValue::UChar(1)));
    }

    /// R9-52: C `monitorStatus` refreshes TSIZ from `getTraceIOTruncateSize`
    /// (asynRecord.c:1100) and writes TFIL = "Unknown" when the port's trace
    /// sink is no longer the one this record installed (:1119-1124). Neither
    /// readback was refreshed on any path.
    #[test]
    fn monitor_status_refreshes_tsiz_and_flags_a_foreign_trace_file() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TsizDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        trace.set_io_truncate_size(Some(port_name), 17).unwrap();
        rec.monitor_status();
        assert_eq!(rec.tsiz, 17);
        assert_eq!(rec.tfil, "Unknown", "the trace file did not change");

        // The record's own TFIL write must NOT read back as foreign.
        rec.tfil = "<stdout>".to_string();
        rec.special("TFIL", true, &Default::default()).unwrap();
        rec.monitor_status();
        assert_eq!(rec.tfil, "<stdout>");

        // A foreign `asynSetTraceFile` re-points the sink: C cannot know the
        // new name, so TFIL becomes "Unknown".
        trace.set_trace_file(Some(port_name), TraceFile::Stderr);
        rec.monitor_status();
        assert_eq!(rec.tfil, "Unknown");
    }

    /// C refreshes the putting record's status from `exceptCallback`, which
    /// only an announced set reaches (asynRecord.c:451, :911-916). A TSIZ the
    /// port refuses announces nothing (asynManager.c:2963-2975), so TSIZ keeps
    /// the value put and the port keeps its size: `caput A.TSIZ -1` after 100
    /// reads A.TSIZ -1, B.TSIZ 100.
    #[test]
    fn a_refused_tsiz_keeps_the_put_value_and_the_port_size() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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

        let port_name = "refused_tsiz";
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        assert!(rec.except_cb.is_some(), "the refresh under test is armed");
        rec.tsiz = 100;
        rec.special("TSIZ", true, &Default::default()).unwrap();
        assert_eq!(rec.tsiz, 100);

        rec.tsiz = -1;
        rec.special("TSIZ", true, &Default::default()).unwrap();
        assert_eq!(rec.tsiz, -1, "no exception, so no monitorStatus refresh");
        assert_eq!(trace.snapshot(Some(port_name), None).io_truncate_size, 100);
    }

    #[test]
    fn test_register_and_get_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
                    InterfaceMask::NONE,
                    InterfaceMask::NONE,
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

        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestDriver::new()), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            "test_asyn_rec".into(),
            interrupts,
            actor_link,
            trace.clone(),
        );

        register_port("test_asyn_rec", handle, &crate::registry::open_owner()).unwrap();

        let entry = crate::registry::get_port("test_asyn_rec");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().handle.port_name(), "test_asyn_rec");
    }

    /// Regression.
    ///
    /// On a multi-device port a record with ADDR >= 0 must route trace
    /// controls to the (PORT,ADDR) device trace state, not the port-wide
    /// state (C findTracePvt, asynManager.c:548-550). A record adjusting
    /// device 3 must not change device 4 or the port default.
    /// C `connectDevice` adds `exceptCallback` on the record's own user
    /// (asynRecord.c:1269), so `asynReport` counts it on the `dpCommon` PORT/ADDR
    /// bound — `exceptionUsers 1` on that device, 0 on the port — and a rebind
    /// moves it.
    #[test]
    fn the_exception_callback_is_counted_on_the_bound_device() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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

        let port_name = "test_except_users_md";
        let flags = PortFlags {
            multi_device: true,
            ..PortFlags::default()
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(MdDriver(PortDriverBase::new(
                port_name,
                2,
                flags,
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let mut handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace,
        );
        handle.set_capabilities(true, 2, crate::port::port_get_address);
        register_port(port_name, handle.clone(), &crate::registry::open_owner()).unwrap();
        let users = |report: &str| {
            report
                .lines()
                .filter_map(|l| l.split("exceptionUsers ").nth(1))
                .map(|rest| rest.split(' ').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.addr = 0;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);
        // The port's line, then device 0's.
        assert_eq!(users(&handle.report_blocking(1).unwrap()), ["0", "1"]);

        rec.addr = 1;
        let _ = rec.connect_device();
        assert_eq!(
            users(&handle.report_blocking(1).unwrap()),
            ["0", "0", "1"],
            "the rebind moved it to device 1"
        );
    }

    #[test]
    fn trace_controls_route_to_device_on_multi_device_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(MdDriver(PortDriverBase::new(
                port_name,
                4,
                flags,
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let mut handle =
            PortHandle::new(tx, port_name.into(), interrupts, actor_link, trace.clone());
        handle.set_capabilities(true, 4, crate::port::port_get_address);
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.addr = 3;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);
        assert!(rec.trace_rung().addr == Some(3));

        // Apply a device-3 trace mask through the record's TMSK path.
        rec.apply_trace_mask(TraceMask::ERROR | TraceMask::FLOW);
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
        assert!(rec0.trace_rung().addr.is_none());
    }

    /// Regression (C `findDpCommon`, asynManager.c:538-545).
    ///
    /// On an `ASYN_MULTIDEVICE` port the `dpCommon` a record reads and writes is
    /// the DEVICE's whenever its ADDR names one, so ENBL, CNCT, AUCT and the
    /// trace mask that a put at ADDR 0 lands on must be invisible at ADDR 1 and
    /// at the port's own slot. A single-device port has no device to pick —
    /// `locateDevice` returns 0 for `!(attributes&ASYN_MULTIDEVICE)`
    /// (asynManager.c:576) — so there the same puts stay port-wide and ADDR 0
    /// *is* the port, one shared slot rather than a device numbered 0.
    #[test]
    fn dp_common_state_is_per_device_only_on_a_multi_device_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct DpDriver(PortDriverBase);
        impl PortDriver for DpDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        fn start(
            port_name: &str,
            multi_device: bool,
            max_addr: usize,
        ) -> crate::registry::PortEntry {
            let flags = PortFlags {
                multi_device,
                ..PortFlags::default()
            };
            let interrupts = Arc::new(InterruptManager::new());
            let (tx, rx) = mpsc::unbounded_channel();
            let actor = PortActor::new(
                Box::new(DpDriver(PortDriverBase::new(
                    port_name,
                    max_addr,
                    flags,
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ))),
                rx,
            );
            let actor_link = actor.link();
            std::thread::spawn(actor.registered());
            let mut handle = PortHandle::new(
                tx,
                port_name.into(),
                interrupts,
                actor_link,
                Arc::new(TraceManager::new()),
            );
            handle.set_capabilities(multi_device, max_addr as i32, crate::port::port_get_address);
            register_port(port_name, handle, &crate::registry::open_owner()).unwrap();
            crate::registry::get_port(port_name).expect("the port is registered")
        }

        fn record(port_name: &str, addr: i32) -> AsynRecord {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            rec.addr = addr;
            rec.connect_device()
                .unwrap_or_else(|e| panic!("{port_name} addr {addr} must bind: {e:?}"));
            rec
        }

        // --- multi-device: one dpCommon per address -------------------------
        let md_name = "test_dpc_md";
        let md = start(md_name, true, 4);
        let mut rec0 = record(md_name, 0);
        let mut rec1 = record(md_name, 1);
        assert_eq!(
            (
                rec0.enbl, rec0.cnct, rec0.auct, rec1.enbl, rec1.cnct, rec1.auct
            ),
            (1, 1, 1, 1, 1, 1),
            "both records start from the port's own dpCommon"
        );

        // AUCT first: a device whose auto-connect is still on can be reconnected
        // under the CNCT put below, which would erase what that put proves.
        rec0.auct = 0;
        rec0.special("AUCT", true, &Default::default()).unwrap();
        assert!(!md.handle.is_auto_connect_blocking(0).unwrap());
        assert!(md.handle.is_auto_connect_blocking(1).unwrap());
        assert!(md.handle.is_auto_connect_blocking(-1).unwrap());

        rec0.cnct = 0;
        rec0.special("CNCT", true, &Default::default()).unwrap();
        assert!(!md.handle.is_connected_blocking(0).unwrap());
        assert!(md.handle.is_connected_blocking(1).unwrap());
        assert!(md.handle.is_connected_blocking(-1).unwrap());

        rec0.enbl = 0;
        rec0.special("ENBL", true, &Default::default()).unwrap();
        assert!(!md.handle.is_enabled_blocking(0).unwrap());
        assert!(md.handle.is_enabled_blocking(1).unwrap());
        assert!(md.handle.is_enabled_blocking(-1).unwrap());

        // TB4 is the FLOW bit of `traceMask` (asynRecord.c's TB0..TB5).
        rec0.tb4 = 1;
        rec0.special("TB4", true, &Default::default()).unwrap();
        assert!(
            md.handle
                .trace()
                .is_enabled_device(md_name, 0, TraceMask::FLOW)
        );
        assert!(
            !md.handle
                .trace()
                .is_enabled_device(md_name, 1, TraceMask::FLOW)
        );
        assert!(!md.handle.trace().is_enabled(md_name, TraceMask::FLOW));

        // What the operator actually sees: the ADDR 1 record's own readback,
        // C `monitorStatus` (asynRecord.c:1066-1099) on its own `pasynUser`.
        rec1.monitor_status();
        assert_eq!(
            (rec1.enbl, rec1.cnct, rec1.auct, rec1.tb4),
            (1, 1, 1, 0),
            "nothing done at ADDR 0 may show at ADDR 1"
        );
        rec0.monitor_status();
        assert_eq!(
            (rec0.enbl, rec0.cnct, rec0.auct, rec0.tb4),
            (0, 0, 0, 1),
            "and ADDR 0 reads back exactly what was put there"
        );

        // --- single device: ADDR 0 and the port are one slot -----------------
        let sd_name = "test_dpc_sd";
        let sd = start(sd_name, false, 1);
        let mut rec = record(sd_name, 0);
        assert!(
            rec.trace_rung().addr.is_none(),
            "ADDR 0 on a single-device port must not become a device slot"
        );

        rec.enbl = 0;
        rec.special("ENBL", true, &Default::default()).unwrap();
        assert!(!sd.handle.is_enabled_blocking(0).unwrap());
        assert!(
            !sd.handle.is_enabled_blocking(-1).unwrap(),
            "the port itself went down, because there is nothing else to go down"
        );

        rec.tb4 = 1;
        rec.special("TB4", true, &Default::default()).unwrap();
        assert!(sd.handle.trace().is_enabled(sd_name, TraceMask::FLOW));
        assert!(
            sd.handle
                .trace()
                .is_enabled_device(sd_name, 0, TraceMask::FLOW)
        );
    }

    /// C `special()` hands CNCT, the options and IEOS/OEOS to
    /// `asynCallbackSpecial` through `queueRequest` (asynRecord.c:528-578),
    /// which refuses a user on no port with its own text (asynManager.c:
    /// 1528-1531): ERRS carries it, and no callback — so no `monitorStatus` —
    /// runs. One case per arm the option put in the as5 pin does not cover.
    #[test]
    fn a_portless_special_put_is_refused_at_its_queue_request() {
        for field in ["IEOS", "OEOS", "CNCT"] {
            let mut rec = AsynRecord::default();
            rec.cnct = 1;
            rec.special(field, true, &Default::default()).unwrap();
            assert_eq!(
                rec.errs, "asynManager::queueRequest asynUser not associated with a port",
                "{field}"
            );
            let posts = named(rec.take_cycle_posted_fields());
            assert_eq!(posts, [("ERRS", CyclePostMask::ValueLog)], "{field}");
        }
    }

    /// A record whose connect failed has a user on no port, and C's
    /// `findTracePvt` gives that user the global rung (asynManager.c:548-553):
    /// `setTraceFile` / `setTraceIOTruncateSize` write it and `monitorStatus`
    /// reads it, while the mask setters refuse the user. The oracles: as5,
    /// `caput A.PORT NOSUCH` posts TMSK 1 and TFIL "Unknown" off
    /// `asynRecPvt.old`'s zeroes, and `caput A.TFIL /nonexistent/dir/f` leaves
    /// ERRS `Error opening trace file: /nonexistent/dir/f`; as12, after `caput
    /// A.TMSK 17; caput A.TSIZ 77` a second portless record reads TMSK 1 and
    /// TSIZ 77.
    ///
    /// One case per boundary: an arm that opens a file, an arm that writes a
    /// mask, an arm that writes the truncate size, the read, and the read's
    /// first sample against C's NULL `old.traceFd`.
    #[test]
    fn a_portless_record_reads_and_writes_the_global_trace() {
        let global = crate::manager::PortManager::global();
        let trace = global.trace_manager();
        let mut rec = AsynRecord::default();
        rec.tfil = "/nonexistent/dir/f".into();
        rec.special("TFIL", true, &Default::default()).unwrap();
        assert_eq!(rec.errs, "Error opening trace file: /nonexistent/dir/f");

        let before = trace.get_trace_mask(None);
        let flow = TraceMask::ERROR | TraceMask::FLOW;
        assert_ne!(before, flow);
        rec.tmsk = flow.bits() as i32;
        rec.special("TMSK", true, &Default::default()).unwrap();
        let size_before = trace.snapshot(None, None).io_truncate_size;
        rec.tsiz = 77;
        rec.special("TSIZ", true, &Default::default()).unwrap();

        let mut fresh = AsynRecord::default();
        fresh.tfil = String::new();
        fresh.read_trace_state();
        trace.set_io_truncate_size(None, size_before).unwrap();
        assert_eq!(rec.errs, "", "the refused mask write reports nothing");
        assert_eq!(
            fresh.tmsk,
            before.bits() as i32,
            "the global mask, untouched"
        );
        assert_eq!(fresh.tsiz, 77, "the global truncate size, written");
        assert_eq!(
            fresh.tfil, TFIL_UNKNOWN,
            "a first sample of any sink but errlog moved off NULL"
        );
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
        use crate::port::InterfaceMask;
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_link, trace.clone());
        // Non-default info mask set on the manager BEFORE the record connects.
        trace.set_trace_info_mask(
            Some(port_name),
            TraceInfoMask::SOURCE | TraceInfoMask::THREAD,
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        use crate::port::InterfaceMask;
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(tx, port_name.into(), interrupts, actor_link, trace.clone());
        // Wire the exception sink as a real IOC would (PortManager installs
        // it); without it the record's subscription is a no-op.
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::TIME);
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                let n = PAYLOAD.len().min(buf.len());
                buf[..n].copy_from_slice(&PAYLOAD[..n]);
                crate::interpose::OctetReadResult::ok(n, EomReason::END)
            }
        }

        let port_name = "test_ifmt_octet_read";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(OctetDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                // 100 CRLF pairs: 400 escaped characters into a 40-byte TINP.
                let payload: Vec<u8> = b"\r\n".repeat(100);
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                crate::interpose::OctetReadResult::ok(n, EomReason::END)
            }
            fn set_input_eos(
                &mut self,
                _user: &AsynUser,
                _eos: &[u8],
            ) -> crate::error::AsynResult<()> {
                Ok(())
            }
            fn get_input_eos(&self, _user: &AsynUser) -> AsynResult<Vec<u8>> {
                // 12 escaped characters into a 10-byte inputEosTranslate.
                Ok(b"\r\n\r\n\r\n".to_vec())
            }
        }

        let port_name = "r17_48_escape_bounds";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(LongDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        rec.special("IEOS", true, &Default::default()).unwrap();
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
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                crate::interpose::OctetReadResult::failed(AsynError::Status {
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(FailingOctetDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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

        // Write failure: NAWT reset to 0 (C assigns nbytesTransferred=0).
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
    /// :1391) and the UInt32 / Float64 twins (:1414/:1429, :1452/:1463) — with
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
        use crate::port::InterfaceMask;
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(AllFailDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION
                    | InterfaceMask::INT32
                    | InterfaceMask::UINT32_DIGITAL
                    | InterfaceMask::FLOAT64
                    | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let reg = |iface: InterfaceType, tmod: TransferMode| -> String {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            let _ = rec.connect_device();
            rec.iface = iface as i32;
            rec.tmod = tmod as i32;
            rec.process().unwrap();
            rec.errs.to_string()
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
        opt.special("LBAUD", true, &Default::default()).unwrap();
        assert_eq!(opt.errs, "Error setting option, opt boom");

        opt.ieos = "\\n".to_string();
        opt.special("IEOS", true, &Default::default()).unwrap();
        assert_eq!(opt.errs, "Error setting input eos, ieos boom");

        opt.oeos = "\\r".to_string();
        opt.special("OEOS", true, &Default::default()).unwrap();
        assert_eq!(opt.errs, "Error setting output eos, oeos boom");
        // The failed EOS sets fall through to the C `getEos` re-read (R9-47),
        // which finds no EOS in this driver and empties both fields.
        assert_eq!(opt.ieos, "");
        assert_eq!(opt.oeos, "");
    }

    /// C reads a register straight into its input field (asynRecord.c:1385,
    /// :1421, :1457), so a failed read still writes it: a parameter's status
    /// fails `asynPortDriver::read*` with the stored value copied out, and a
    /// parameter-library failure zeroes it first (asynPortDriver.cpp:304,
    /// :359, :386).
    #[test]
    fn a_failed_register_read_writes_the_input_field_as_c() {
        use crate::error::AsynStatus;
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct ParamPort(PortDriverBase);
        impl PortDriver for ParamPort {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let port_name = "test_failed_register_read_field";
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION
                | InterfaceMask::INT32
                | InterfaceMask::UINT32_DIGITAL
                | InterfaceMask::FLOAT64
                | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        let i32p = base.create_param("I32", ParamType::Int32).unwrap();
        let u32p = base.create_param("U32", ParamType::UInt32Digital).unwrap();
        let f64p = base.create_param("F64", ParamType::Float64).unwrap();
        base.params.set_int32(i32p, 0, 42).unwrap();
        base.params.set_uint32(u32p, 0, 0x0F, 0xFF, 0).unwrap();
        base.params.set_float64(f64p, 0, 2.5).unwrap();
        for p in [i32p, u32p, f64p] {
            base.params.set_status(p, 0, AsynStatus::Error).unwrap();
        }
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(ParamPort(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let read = |iface: InterfaceType, reason: usize| -> AsynRecord {
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            let _ = rec.connect_device();
            rec.resolved_reason = reason;
            rec.iface = iface as i32;
            rec.tmod = TransferMode::Read as i32;
            rec.ui32mask = 0xFF;
            rec.i32inp = 9;
            rec.ui32inp = 9;
            rec.f64inp = 9.0;
            rec.process().unwrap();
            rec
        };

        let rec = read(InterfaceType::Int32, i32p);
        assert_eq!(rec.i32inp, 42);
        assert_eq!(
            rec.errs.to_string(),
            "Int32 read error, asynPortDriver:readInt32: status=3, function=0, name=I32, value=42"
        );
        assert_eq!(read(InterfaceType::UInt32Digital, u32p).ui32inp, 0x0F);
        assert_eq!(read(InterfaceType::Float64, f64p).f64inp, 2.5);

        // Wrong type: paramList zeroes the out-parameter before it fails.
        let rec = read(InterfaceType::Int32, f64p);
        assert_eq!(rec.i32inp, 0);
        assert_eq!(
            rec.errs.to_string(),
            "Int32 read error, asynPortDriver:readInt32: status=8, function=2, name=F64, value=0"
        );
        assert_eq!(read(InterfaceType::UInt32Digital, i32p).ui32inp, 0);
        assert_eq!(read(InterfaceType::Float64, i32p).f64inp, 0.0);
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
        use crate::port::InterfaceMask;
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
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
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
            fn get_input_eos(&self, _user: &AsynUser) -> AsynResult<Vec<u8>> {
                Ok(b"\r\n".to_vec())
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
            fn get_output_eos(&self, _user: &AsynUser) -> AsynResult<Vec<u8>> {
                Ok(b"\n".to_vec())
            }
        }

        let port_name = "test_option_eos_reread";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(StubbornDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();

        // The operator asks for 115200. The set succeeds; the driver stays at
        // 9600. C's getOptions fall-through snaps LBAUD (and the BAUD menu
        // index derived from it) back to what the port really runs at.
        rec.lbaud = 115_200;
        rec.special("LBAUD", true, &Default::default()).unwrap();
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
        rec.special("IEOS", true, &Default::default()).unwrap();
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
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                buf[..2].copy_from_slice(b"OK");
                crate::interpose::OctetReadResult::ok(2, EomReason::END)
            }
        }

        let port_name = "test_flush_failure_discarded";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(FlushFailsDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
    /// overwrites the earlier `"Error %s"` at :1584-1585, which never reaches the
    /// operator). The port emitted `"read: {e}"`, which carries neither the
    /// status word nor the count and prefixes the Rust status debug onto the
    /// driver text.
    #[test]
    fn octet_read_error_errs_matches_c_report_error() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::{EomReason, OctetReadResult};
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                self.n += 1;
                match self.n {
                    1 => OctetReadResult::failed(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
                    }),
                    2 => OctetReadResult::failed(AsynError::Status {
                        status: AsynStatus::Overflow,
                        message: "buffer full".into(),
                    }),
                    _ => {
                        buf[..2].copy_from_slice(b"xy");
                        OctetReadResult {
                            status: Err(AsynError::Status {
                                status: AsynStatus::Error,
                                message: "device fault".into(),
                            }),
                            ..OctetReadResult::ok(2, EomReason::empty())
                        }
                    }
                }
            }
        }

        let port_name = "test_octet_read_errs_text";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(StatusDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                n: 0,
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
    /// `nbytesTransferred` regardless of the read status (asynRecord.c:1591-1629
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
        use crate::interpose::{EomReason, OctetReadResult};
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                buf[..3].copy_from_slice(b"abc");
                buf[3] = 0;
                OctetReadResult {
                    status: Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
                    }),
                    ..OctetReadResult::ok(3, EomReason::empty())
                }
            }
        }

        let port_name = "test_octet_partial_read";
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(PartialThenTimeout(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        rec.iface = 0; // asynOctet
        rec.tmod = TransferMode::Read as i32;
        rec.imax = 256;
        rec.ifmt = ASYN_FMT_ASCII;
        rec.process().unwrap();

        assert_eq!(
            rec.ainp, "abc",
            "the partial line reaches AINP (C :1503-1505)"
        );
        assert_eq!(rec.nord, 3, "NORD = nbytesTransferred (C :1627)");
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
    /// *readback*: `monitorStatus` (:1090-1094) assigns it from `isConnected`.
    ///
    /// Before the fix CNCT was a duplicate of PCNCT: a CNCT=0 put detached the
    /// record and left the socket open, CNCT=1 could not raise a dropped link,
    /// and `connect_device` latched CNCT=1 on a mere attach — so a
    /// registered-but-disconnected port reported a live wire.
    #[test]
    fn cnct_drives_the_transport_and_pcnct_the_attachment() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(CountingDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                connects: connects.clone(),
                disconnects: disconnects.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        rec.special("CNCT", true, &Default::default()).unwrap();
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
            rec.device_user.device().is_some(),
            "CNCT must not drop the port binding"
        );

        // CNCT=1 on the disconnected port connects it: the record's special
        // user rides the Connect queue, and on a single-device port it has no
        // device at its ADDR (`locateDevice`, asynManager.c:576), so C's
        // `checkPortConnect` waiver holds (:1538-1540).
        rec.cnct = 1;
        rec.special("CNCT", true, &Default::default()).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            base_connects + 1,
            "CNCT=1 must connect the driver's transport"
        );
        assert_eq!(rec.errs, "");
        assert_eq!(rec.cnct, 1);

        // C's isConnected gate: re-putting the state the port is already in
        // issues no driver call at all (asynRecord.c:865-889).
        rec.cnct = 1;
        rec.special("CNCT", true, &Default::default()).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            base_connects + 1,
            "an already-connected port must not be re-connected"
        );

        // PCNCT=0 detaches the record and leaves the transport alone.
        let disconnects_before = disconnects.load(Ordering::SeqCst);
        rec.pcnct = 0;
        rec.special("PCNCT", true, &Default::default()).unwrap();
        assert!(
            rec.device_user.device().is_none(),
            "PCNCT=0 detaches the record"
        );
        assert_eq!(
            disconnects.load(Ordering::SeqCst),
            disconnects_before,
            "PCNCT must not touch the driver's transport"
        );
        assert_eq!(
            rec.cnct, 0,
            "detached: isConnected has no device to report on (C :1094)"
        );
    }

    /// R14-46: a request the queue gate refuses never ran, and the record must
    /// report it as a refusal — not as a callback that did something.
    ///
    /// C `special()` (asynRecord.c:571-578): when `queueRequest` returns non-zero
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
        use crate::port::InterfaceMask;
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
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
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
            ) -> crate::interpose::OctetReadResult {
                self.log.lock().unwrap().reads += 1;
                let data = b"hi";
                buf[..data.len()].copy_from_slice(data);
                crate::interpose::OctetReadResult::ok(
                    data.len(),
                    crate::interpose::EomReason::empty(),
                )
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(GateDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                log: log.clone(),
                connects: connects.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        assert_eq!(rec.baud, baud_choice_index("9600"), "the port runs at 9600");
        let entry_handle = rec.device_user.device().unwrap().handle.clone();

        // --- Control: the callback RAN and the driver refused the value. C
        // reports "Error setting option, %s" (:1828-1830) *and* keeps the whole
        // callback tail — the getOptions fall-through snaps BAUD back to 9600.
        log.lock().unwrap().option_sets.clear();
        rec.baud = 0; // menu index 0 = "Unknown"
        rec.special("BAUD", true, &Default::default()).unwrap();
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
        // (asynManager.c:1543-1548 — no waiver reaches it).
        entry_handle.set_enable(-1, false).unwrap();
        log.lock().unwrap().option_sets.clear();
        rec.baud = baud_choice_index("19200");
        rec.special("BAUD", true, &Default::default()).unwrap();
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
        // (checkPortConnect, asynManager.c:1549-1554) — an EOS put carries no
        // waiver, so it is refused with the port enabled but the line down.
        entry_handle.set_enable(-1, true).unwrap();
        entry_handle.disconnect_blocking().unwrap();
        log.lock().unwrap().eos_sets.clear();
        rec.ieos = "\\n".to_string();
        rec.special("IEOS", true, &Default::default()).unwrap();
        assert!(
            log.lock().unwrap().eos_sets.is_empty(),
            "a refused EOS put must not reach the driver"
        );
        assert!(
            rec.errs.contains("not connected"),
            "ERRS is the gate's errorMessage: {}",
            rec.errs
        );

        // --- Boundary 4: the CNCT put refused, by the disabled port — the one
        // refusal the Connect queue's waiver does not lift (:1543-1548). No
        // connect is attempted, and with no callback there is no monitorStatus
        // tail: CNCT keeps the operator's value instead of being snapped back.
        entry_handle.set_enable(-1, false).unwrap();
        let connects_before = connects.load(Ordering::SeqCst);
        rec.cnct = 1;
        rec.special("CNCT", true, &Default::default()).unwrap();
        assert_eq!(
            connects.load(Ordering::SeqCst),
            connects_before,
            "a refused CNCT put must not touch the wire"
        );
        assert!(
            rec.errs.contains("disabled"),
            "ERRS is the gate's errorMessage, not a callback-shaped text: {}",
            rec.errs
        );
        assert_eq!(rec.cnct, 1, "no callback ran, so no monitorStatus tail");
    }

    /// C `reportError` prints its text at ASYN_TRACE_ERROR unless the status is
    /// success or timeout (asynRecord.c:2037-2040). Oracle as15 — record `A` on
    /// echoDriver port `P`, trace mask 1, info mask PORT — printed exactly:
    ///
    /// ```text
    /// [P,-1,0] A: queueRequest failed for special.   <- caput A.BAUD, P disabled
    /// [P,-1,0] A: port P disabled
    /// [P,-1,0] A: No asynInt32 interface              <- A.PROC, P enabled
    /// ```
    ///
    /// and nothing for the `queueRequest failed` of `A.PROC` on the disabled
    /// port, which C reports with `asynSuccess` (:355).
    #[test]
    fn report_error_prints_the_c_trace_lines() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use epics_base_rs::server::database::PvDatabase;
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

        let port_name = "as15_P";
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(D(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let mut handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        // echoDriver's interfaces: asynCommon and asynOctet.
        handle.set_interfaces(crate::interfaces::octet_transport_capabilities());
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();
        let path = std::env::temp_dir().join(format!("as15-trace-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some(port_name),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some(port_name), TraceMask::ERROR);
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::PORT);

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        let _ = rec.connect_device();
        let db = PvDatabase::new();
        rec.set_async_context("A".into(), db.async_handle());
        let entry_handle = rec.device_user.device().unwrap().handle.clone();

        entry_handle.set_enable(-1, false).unwrap();
        rec.baud = baud_choice_index("9600");
        rec.special("BAUD", true, &Default::default()).unwrap();
        rec.tmod = TransferMode::NoIo as i32;
        rec.process().unwrap();
        assert_eq!(rec.errs, "queueRequest failed");
        entry_handle.set_enable(-1, true).unwrap();
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        rec.process().unwrap();

        let printed = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            printed,
            "[as15_P,-1,0] A: queueRequest failed for special.\n\
             [as15_P,-1,0] A: port as15_P disabled\n\
             [as15_P,-1,0] A: No asynInt32 interface\n"
        );
    }

    /// C's interface refusals, the NOWT/NRRD clamps and the UCMD/ACMD reset
    /// all happen inside `asynCallbackProcess` (asynRecord.c:821-826, :1328-1360,
    /// :1499, :1514, :1647), so a cycle `queueRequest` refused does none of
    /// them: ERRS reads "queueRequest failed" and the fields keep their puts.
    #[test]
    fn a_refused_cycle_leaves_the_callbacks_work_undone() {
        use crate::interfaces::octet_transport_capabilities;
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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

        let port_name = "refused_cycle";
        let (rt, _jh) = create_port_runtime_unclaimed(
            OctetTransport(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        let port = rec.device_user.device().unwrap().handle.clone();
        rec.tmod = TransferMode::Read as i32;
        rec.ofmt = ASYN_FMT_BINARY;
        rec.ifmt = ASYN_FMT_BINARY;
        (rec.omax, rec.imax) = (8, 16);

        let process = |rec: &mut AsynRecord, errs: &str, alarm| {
            rec.process().unwrap();
            assert_eq!(rec.errs, errs);
            assert_eq!(read_alarm(rec), alarm);
        };
        let refused = (alarm_status::STATE_ALARM, AlarmSeverity::Minor);
        let no_iface = (alarm_status::COMM_ALARM, AlarmSeverity::Major);

        port.set_enable(-1, false).unwrap();
        rec.iface = InterfaceType::Int32 as i32;
        process(&mut rec, "queueRequest failed", refused);
        rec.iface = InterfaceType::Octet as i32;
        (rec.nowt, rec.nrrd) = (500, 900);
        process(&mut rec, "queueRequest failed", refused);
        assert_eq!((rec.nowt, rec.nrrd), (500, 900));
        rec.ucmd = 1;
        process(&mut rec, "queueRequest failed", refused);
        assert_eq!(rec.ucmd, 1, "the command waits for a cycle that runs");

        // Enabled, the callback runs, refuses the command and resets it.
        port.set_enable(-1, true).unwrap();
        process(&mut rec, "No asynGpib interface", no_iface);
        assert_eq!(rec.ucmd, 0);
        rec.iface = InterfaceType::Int32 as i32;
        process(&mut rec, "No asynInt32 interface", no_iface);
        rec.iface = InterfaceType::Octet as i32;
        rec.process().unwrap();
        assert_eq!((rec.nowt, rec.nrrd), (8, 16));
    }

    /// C `queueRequest` on a port without ASYN_CANBLOCK gates once, prints
    /// "%s queueRequest synchronous" once, and runs the whole callback under
    /// `synchronousLock` (asynManager.c:1555-1584). Oracle (echoDriver delay 0,
    /// FLOW): one such line per `A.PROC` and one per `A.BAUD` put. So a write
    /// that drops the link is still followed by the read, which reports the
    /// driver's error (asynRecord.c:1593-1599), not a refusal.
    #[test]
    fn a_synchronous_port_runs_each_callback_as_one_request() {
        use crate::interpose::OctetReadResult;
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct DropsOnWrite(PortDriverBase, Arc<Mutex<Vec<&'static str>>>);
        impl PortDriver for DropsOnWrite {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                self.1.lock().unwrap().push("flush");
                Ok(())
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.1.lock().unwrap().push("write");
                self.0.set_connected(false);
                Ok(data.len())
            }
            fn io_read_octet(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> OctetReadResult {
                self.1.lock().unwrap().push("read");
                OctetReadResult::failed(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "link down".into(),
                })
            }
        }

        let port_name = "sync_one_request";
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION
                | InterfaceMask::INT32
                | InterfaceMask::UINT32_DIGITAL
                | InterfaceMask::FLOAT64
                | InterfaceMask::OCTET
                | InterfaceMask::DRV_USER,
            InterfaceMask::NONE,
        );
        base.set_connected(true);
        base.init_auto_connect(false);
        let (rt, _jh) = create_port_runtime_unclaimed(
            DropsOnWrite(base, log.clone()),
            RuntimeConfig::default(),
        )
        .unwrap();
        let trace = rt.port_handle().trace().clone();
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("sync-one-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some(port_name),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_info_mask(Some(port_name), TraceInfoMask::empty());
        trace.set_trace_mask(Some(port_name), TraceMask::ERROR | TraceMask::FLOW);
        let gates = || {
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("sync_one_request queueRequest synchronous\n")
                .count()
        };

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        let before = gates();
        rec.baud = baud_choice_index("9600");
        rec.special("BAUD", true, &Default::default()).unwrap();
        assert_eq!(
            gates() - before,
            1,
            "one request for the set and its readback"
        );

        rec.iface = InterfaceType::Octet as i32;
        rec.tmod = TransferMode::WriteRead as i32;
        rec.aout = "hi".to_string();
        let before = gates();
        rec.process().unwrap();
        assert_eq!(gates() - before, 1, "one request for the whole cycle");
        let _ = std::fs::remove_file(&path);
        assert_eq!(*log.lock().unwrap(), ["flush", "write", "read"]);
        assert_eq!(rec.errs, "error  nread 0 link down");
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::READ_ALARM, AlarmSeverity::Major)
        );
    }

    /// A body that logs how its request ended.
    #[derive(Default)]
    struct EndLog(Mutex<Vec<&'static str>>);

    impl crate::request::CellBody for EndLog {
        fn process(&self, _port: &mut dyn crate::request::PortCalls) {
            self.0.lock().unwrap().push("processed");
        }
        fn unrun(&self, why: crate::request::Unrun) {
            use crate::request::Unrun;
            self.0.lock().unwrap().push(match why {
                Unrun::Canceled => "canceled",
                Unrun::TimedOut => "timed out",
                Unrun::Refused(_) => "refused",
            });
        }
        fn done(self: Arc<Self>) {}
    }

    impl EndLog {
        /// The log once it holds `n` entries, or after 5 s.
        fn wait_for(&self, n: usize) -> Vec<&'static str> {
            let started = std::time::Instant::now();
            while self.0.lock().unwrap().len() < n
                && started.elapsed() < std::time::Duration::from_secs(5)
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.0.lock().unwrap().clone()
        }
    }

    /// A port whose thread is inside a driver read that drops the line on
    /// release, with two cells queued behind it: `timed` with a 100 ms queue
    /// timeout and `waits` with none. The line stays down until `line_up`, and
    /// the production loop retries it every 20 ms (asynManager.c:3283-3314).
    struct JammedPort {
        release: std::sync::mpsc::Sender<()>,
        jam: Option<std::thread::JoinHandle<RequestResult>>,
        line_up: Arc<std::sync::atomic::AtomicBool>,
        timed: Arc<EndLog>,
        waits: Arc<EndLog>,
        waits_cell: Arc<crate::request::RequestCell>,
        _timed_cell: Arc<crate::request::RequestCell>,
        _shutdown: tokio::sync::mpsc::Sender<()>,
        /// The port stops once nobody holds a handle to it.
        handle: PortHandle,
    }

    impl JammedPort {
        fn new(name: &str) -> Self {
            use crate::interrupt::InterruptManager;
            use crate::port::InterfaceMask;
            use crate::port::{PortDriver, PortDriverBase, PortFlags};
            use crate::port_actor::PortActor;
            use std::sync::mpsc;
            use std::time::Duration;

            struct Drops {
                base: PortDriverBase,
                entered: mpsc::SyncSender<()>,
                release: Mutex<mpsc::Receiver<()>>,
                line_up: Arc<std::sync::atomic::AtomicBool>,
            }
            impl PortDriver for Drops {
                fn base(&self) -> &PortDriverBase {
                    &self.base
                }
                fn base_mut(&mut self) -> &mut PortDriverBase {
                    &mut self.base
                }
                fn read_int32(&mut self, _user: &AsynUser) -> crate::error::AsynResult<i32> {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                    // The line drops while the port thread is in the driver.
                    self.base.set_connected(false);
                    Ok(0)
                }
                fn connect(&mut self, _user: &AsynUser) -> crate::error::AsynResult<()> {
                    if !self.line_up.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(crate::error::AsynError::Status {
                            status: crate::error::AsynStatus::Error,
                            message: "line down".into(),
                        });
                    }
                    self.base.set_connected(true);
                    Ok(())
                }
            }

            let (entered_tx, entered) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::channel();
            let line_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // Parking on a dropped line is the port thread's (:808-810), which
            // only a CANBLOCK port has.
            let flags = PortFlags {
                can_block: true,
                ..PortFlags::default()
            };
            let mut base =
                PortDriverBase::new(name, 1, flags, InterfaceMask::INT32, InterfaceMask::NONE);
            base.seconds_between_port_connect = Duration::from_millis(20);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = PortActor::new(
                Box::new(Drops {
                    base,
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                    line_up: line_up.clone(),
                }),
                rx,
            );
            // The line is up at registration and stays down once it drops.
            let up = std::sync::atomic::Ordering::SeqCst;
            line_up.store(true, up);
            actor.register();
            line_up.store(false, up);
            let link = actor.link();
            // The production loop: the test loop runs no connect-retry timer.
            let (shutdown, shutdown_rx) = tokio::sync::mpsc::channel(1);
            std::thread::spawn(move || actor.run_with_shutdown(shutdown_rx));
            let mut handle = PortHandle::new(
                tx,
                name.into(),
                Arc::new(InterruptManager::new()),
                link,
                Arc::new(TraceManager::new()),
            );
            handle.set_can_block(true);

            let jammer = handle.clone();
            let jam = std::thread::spawn(move || {
                jammer
                    .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                    .unwrap()
            });
            entered.recv().unwrap();
            let timed = Arc::new(EndLog::default());
            let waits = Arc::new(EndLog::default());
            let timed_cell = handle.new_cell(timed.clone());
            let waits_cell = handle.new_cell(waits.clone());
            timed_cell
                .queue(&AsynUser::new(0).with_queue_timeout(Duration::from_millis(100)))
                .unwrap();
            waits_cell.queue(&AsynUser::new(0)).unwrap();
            Self {
                release,
                jam: Some(jam),
                line_up,
                timed,
                waits,
                waits_cell,
                _timed_cell: timed_cell,
                _shutdown: shutdown,
                handle,
            }
        }

        /// The driver read returns and the line is down.
        fn release(&mut self) {
            self.release.send(()).unwrap();
            self.jam.take().unwrap().join().unwrap();
        }
    }

    /// C `portThread` leaves a queued request where it is while its port is
    /// disabled or down (asynManager.c:808-810, :858-863): it runs when the
    /// port comes back, or its queue timer takes it (:649-703). It is never
    /// refused at dequeue — that refusal is `queueRequest`'s, made before it
    /// was queued.
    #[test]
    fn a_cell_queued_before_its_port_went_down_stays_queued() {
        let mut port = JammedPort::new("park_on_down");
        port.release();

        assert_eq!(
            port.timed.wait_for(1),
            ["timed out"],
            "queued until its timer"
        );
        assert!(
            port.waits.0.lock().unwrap().is_empty(),
            "still queued, not refused"
        );
        assert!(port.waits_cell.is_outstanding());

        port.line_up
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            port.waits.wait_for(1),
            ["processed"],
            "runs once the port is back"
        );
    }

    /// C `cancelRequest` unlinks a queued request at once (asynManager.c:1663-1668)
    /// and asynRecord's `special()` reports it then (asynRecord.c:396-404): a
    /// request parked on a down port is reported when cancelled, not when the
    /// port comes back. The port thread, told (`poke`), drops the entry it
    /// parked, which holds the cell.
    #[test]
    fn a_cancel_reports_a_request_parked_on_a_down_port() {
        let mut port = JammedPort::new("cancel_parked");
        port.release();
        assert_eq!(port.timed.wait_for(1), ["timed out"]);
        assert!(port.waits_cell.is_outstanding(), "parked");
        assert_eq!(Arc::strong_count(&port.waits_cell), 2, "the parked entry");

        assert!(port.waits_cell.cancel(), "wasQueued");
        assert_eq!(*port.waits.0.lock().unwrap(), ["canceled"]);
        assert!(!port.waits_cell.is_outstanding());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while Arc::strong_count(&port.waits_cell) != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the port thread kept the cancelled entry"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        port.line_up
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(*port.waits.0.lock().unwrap(), ["canceled"], "never runs");
    }

    /// C `enable` and `autoConnectAsyn` never refuse (asynManager.c:2224-2250,
    /// :2312-2329). With the port thread inside a driver call nothing drains
    /// what the calls owe, and 200 of them are still all taken, allocating
    /// nothing.
    #[test]
    fn enable_and_auto_connect_while_the_port_thread_is_busy_never_refuse() {
        epics_libcom_rs::runtime::task::test_ioc_thread(|| {
            use crate::asyn_record::errs_alloc_tests::thread_allocs;

            let port = JammedPort::new("owed_busy");
            port.handle.set_enable(-1, true).unwrap();
            port.handle.set_auto_connect(-1, true).unwrap();
            let before = thread_allocs();
            for i in 0..100 {
                port.handle.set_enable(-1, i % 2 == 0).unwrap();
                port.handle.set_auto_connect(-1, i % 2 == 0).unwrap();
            }
            assert_eq!(thread_allocs() - before, 0);
        });
    }

    /// C SyncIO queues through `queueLockPort` with a deadline of
    /// `max(queueLockPortTimeout, timeout)` (asynManager.c:1894-1896) and fails
    /// the call when it passes (:972-990): a SyncIO read queued just before its
    /// port drops returns an error at 2 s, not when the port comes back.
    #[test]
    fn a_syncio_call_queued_before_its_port_drops_fails_at_the_lock_port_deadline() {
        use std::time::{Duration, Instant};

        let mut port = JammedPort::new("syncio_park");
        let sio = crate::sync_io::SyncIOHandle::from_handle(
            port.handle.clone(),
            0,
            Duration::from_millis(200),
        );
        let (got_tx, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let r = sio.read_int32(0);
            let _ = got_tx.send((started.elapsed(), r));
        });
        std::thread::sleep(Duration::from_millis(50));
        port.release();
        let (took, r) = got
            .recv_timeout(Duration::from_secs(6))
            .expect("the SyncIO read never returned");
        assert!(r.unwrap_err().is_queue_timeout());
        assert!(took >= crate::user::QUEUE_LOCK_PORT_TIMEOUT, "{took:?}");
    }

    /// The same while the port thread is inside the driver: the cancel does
    /// not wait for it to come out, and takes the entry off the queue itself
    /// (C `ellDelete`, asynManager.c:1667).
    #[test]
    fn a_cancel_reports_while_the_port_thread_is_busy() {
        let mut port = JammedPort::new("cancel_busy");
        assert_eq!(Arc::strong_count(&port.waits_cell), 2, "the queued entry");
        assert!(port.waits_cell.cancel(), "wasQueued");
        assert_eq!(*port.waits.0.lock().unwrap(), ["canceled"]);
        assert_eq!(Arc::strong_count(&port.waits_cell), 1, "unlinked");
        port.release();
        port.line_up
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(*port.waits.0.lock().unwrap(), ["canceled"], "never runs");
    }

    /// R8-48: NAWT is what the *device* took, on both arms. C `performOctetIO`
    /// (asynRecord.c:1524-1556) seeds `nbytesTransferred = 0`, hands it to the
    /// octet chain — which fills it in on success and on failure alike
    /// (`drvAsynSerialPort.c:843` writes `numchars - nleft` on the timeout
    /// break) — and commits it with `nawt = nbytesTransferred` (:1547) *before*
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
        use crate::port::InterfaceMask;
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
            let interrupts = Arc::new(InterruptManager::new());
            let (tx, rx) = mpsc::unbounded_channel();
            let actor = PortActor::new(
                Box::new(ShortWriteDriver {
                    base: PortDriverBase::new(
                        port_name,
                        1,
                        PortFlags::default(),
                        InterfaceMask::OPTION | InterfaceMask::OCTET,
                        InterfaceMask::NONE,
                    ),
                    accept,
                    fail,
                }),
                rx,
            );
            let actor_link = actor.link();
            std::thread::spawn(actor.registered());
            let handle = PortHandle::new(
                tx,
                port_name.into(),
                interrupts,
                actor_link,
                Arc::new(TraceManager::new()),
            );
            register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        assert_eq!(partial.nawt, 3, "NAWT = nbytesTransferred (C :1547)");
        assert!(
            partial.errs.starts_with("Write error, nout=3,"),
            "C :1553 reportError text, got {:?}",
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
        // (`nbytesTransferred != nwrite`), and NAWT is the driver's real count,
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

    /// C's `perform*IO` helpers raise a record alarm severity for every I/O
    /// failure via `recGblSetSevr` (asynRecord.c:1364-1636): octet/register read error ->
    /// READ/MAJOR, register write error -> WRITE/MAJOR, while the octet *write*
    /// branch (:1551-1555) raises none. The asyn record stages the alarm in
    /// `io_alarm` and `check_alarms` commits it to NSEV/NSTA.
    #[test]
    fn io_errors_raise_record_alarm() {
        use crate::error::{AsynError, AsynStatus};
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                _buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                crate::interpose::OctetReadResult::failed(boom())
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
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(FailDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::INT32 | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
    /// for. The requested size IS C's `nread` (asynRecord.c:1513-1517), so it
    /// pins which capacity — `sizeof(ainp)` or IMAX — sized the read.
    fn spawn_fill_port(
        port_name: &'static str,
        fill: usize,
        eom: crate::interpose::EomReason,
    ) -> Arc<Mutex<Option<usize>>> {
        use crate::interpose::EomReason;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                *self.requested.lock().unwrap() = Some(buf.len());
                let n = self.fill.min(buf.len());
                for b in buf[..n].iter_mut() {
                    *b = b'Z';
                }
                crate::interpose::OctetReadResult::ok(n, self.eom)
            }
        }

        let requested = Arc::new(Mutex::new(None));
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(FillDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                fill,
                eom,
                requested: requested.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();
        requested
    }

    /// R9-46: C `asynCallbackProcess` assigns TMOT to the asynUser verbatim —
    /// `pasynUser->timeout = pasynRec->tmot` (asynRecord.c:818) — and the value
    /// is three-valued: `>0` bounded wait, `==0` non-blocking poll
    /// (`drvAsynIPPort.c:792-793` floors the poll to 1 ms;
    /// `drvAsynSerialPort.c:902-905` sets `VMIN=0,VTIME=0`), `<0` wait forever.
    /// The port substituted a 1 s wait for BOTH non-positive cases, so an
    /// operator asking for a poll got a one-second block on a silent device and
    /// one asking to wait forever got an alarm after a second.
    ///
    /// All three values now pass through: `tmot < 0` is the operator asking to
    /// wait forever and reaches the drivers as `None`, the unbounded wait C
    /// builds from it (DRV-42). It used to take the bounded 1 s fallback, so a
    /// record told to wait forever timed out after a second and raised an alarm
    /// C never raises. Boundary-per-case, not scenario-per-case.
    #[test]
    fn tmot_is_passed_through_verbatim() {
        use std::time::Duration;

        let plan_timeout = |tmot: f64| {
            let mut rec = AsynRecord::default();
            rec.tmot = tmot;
            rec.build_io_plan().timeout
        };

        // The defect: a zero TMOT is a poll, not a one-second wait.
        assert_eq!(
            plan_timeout(0.0),
            Some(Duration::ZERO),
            "TMOT=0 is C's non-blocking poll"
        );
        // Positive values were already verbatim; pin them so the owner keeps them.
        assert_eq!(plan_timeout(2.5), Some(Duration::from_millis(2500)));
        assert_eq!(plan_timeout(0.001), Some(Duration::from_millis(1)));
        // DRV-42: negative TMOT is C's "wait forever".
        assert_eq!(
            plan_timeout(-1.0),
            None,
            "negative TMOT is C's unbounded wait, not a bounded default"
        );
        assert_eq!(plan_timeout(-0.001), None);
        // The values `Duration` cannot carry keep the bounded fallback. C casts
        // these with `(int)`, which is undefined behaviour, so there is no C
        // semantics here.
        assert_eq!(plan_timeout(f64::NAN), Some(Duration::from_secs(1)));
        assert_eq!(plan_timeout(f64::INFINITY), Some(Duration::from_secs(1)));
        assert_eq!(
            plan_timeout(f64::NEG_INFINITY),
            Some(Duration::from_secs(1))
        );
    }

    /// R9-46, end to end: the TMOT the operator set is the timeout the driver's
    /// `AsynUser` carries. Pins the value at the C boundary
    /// (`pasynUser->timeout`), not just inside the plan.
    #[test]
    fn tmot_zero_reaches_the_driver_as_a_zero_timeout() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct TimeoutSpy {
            base: PortDriverBase,
            /// Outer `Option`: whether the driver was reached at all. Inner:
            /// the timeout it was reached with, `None` being C's wait-forever.
            seen: Arc<Mutex<Option<Option<std::time::Duration>>>>,
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
            ) -> crate::interpose::OctetReadResult {
                *self.seen.lock().unwrap() = Some(user.timeout);
                buf[0] = b'x';
                crate::interpose::OctetReadResult::ok(1, crate::interpose::EomReason::empty())
            }
        }

        let port_name = "test_tmot_zero_to_driver";
        let seen = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TimeoutSpy {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                seen: seen.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = read_rec(port_name, 0, 40, 0);
        rec.tmot = 0.0;
        rec.process().unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            Some(Some(std::time::Duration::ZERO)),
            "the driver's asynUser must carry the operator's TMOT=0 poll"
        );
    }

    /// DRV-42, the other end of the same boundary: a negative TMOT is C's
    /// "wait forever" and must reach the driver as an unbounded wait. It used
    /// to arrive as the 1 s default, so a record told to wait forever timed out
    /// and alarmed where C waits.
    #[test]
    fn tmot_negative_reaches_the_driver_as_wait_forever() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::user::AsynUser;
        use tokio::sync::mpsc;

        struct TimeoutSpy {
            base: PortDriverBase,
            seen: Arc<Mutex<Option<Option<std::time::Duration>>>>,
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
            ) -> crate::interpose::OctetReadResult {
                *self.seen.lock().unwrap() = Some(user.timeout);
                buf[0] = b'x';
                crate::interpose::OctetReadResult::ok(1, crate::interpose::EomReason::empty())
            }
        }

        let port_name = "test_tmot_forever_to_driver";
        let seen = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TimeoutSpy {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                seen: seen.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = read_rec(port_name, 0, 40, 0);
        rec.tmot = -1.0;
        rec.process().unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            Some(None),
            "the driver's asynUser must carry the operator's wait-forever"
        );
        assert_eq!(
            read_alarm(&mut rec),
            (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
            "waiting forever is not an alarm condition"
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
        use crate::port::InterfaceMask;
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                self.log.lock().unwrap().push("read");
                let resp = b"OK";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                crate::interpose::OctetReadResult::ok(n, EomReason::EOS)
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(PhaseLogDriver {
                base: PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::OCTET,
                    InterfaceMask::NONE,
                ),
                log: log.clone(),
            }),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();
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
            let mut plan = rec.build_io_plan();
            plan.missing = None; // the record has no port to ask
            io_phases(&plan).iter().collect()
        };
        use InterfaceType::{Int32, Octet};
        use IoPhase::{Flush, Read, Write};

        assert_eq!(plan(TransferMode::WriteRead, Octet), [Flush, Write, Read]);
        assert_eq!(plan(TransferMode::Write, Octet), [Write]);
        assert_eq!(plan(TransferMode::Read, Octet), [Read]);
        assert_eq!(plan(TransferMode::Flush, Octet), [Flush]);
        // R18-87: NoI/O is one queue entry with an empty body, not zero queue
        // entries — C `process()` queues without consulting TMOD
        // (asynRecord.c:342-353; its one skip is the `gotValue` I/O Intr
        // readback at `:341`) and the callback is what skips `performIO`
        // (`:827`).
        assert_eq!(plan(TransferMode::NoIo, Octet), [IoPhase::NoIo]);
        assert_eq!(plan(TransferMode::NoIo, Int32), [IoPhase::NoIo]);

        // Register interfaces: no flush phase in C, in any TMOD.
        assert_eq!(plan(TransferMode::WriteRead, Int32), [Write, Read]);
        assert_eq!(plan(TransferMode::Flush, Int32), []);

        // A missing interface is the callback's one phase, whatever TMOD
        // (asynRecord.c:1328-1360).
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Write as i32;
        let phases: Vec<IoPhase> = io_phases(&rec.build_io_plan()).iter().collect();
        assert_eq!(phases, [IoPhase::NoInterface("asynOctet")]);
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
        // while no call on this user has failed.
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
    /// blank, so `process()`'s ERRS clear (:339) was all the
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

    /// The `%s` of an overflow that did not fail is `pasynUser->errorMessage`
    /// (asynRecord.c:1602-1608), which keeps whatever an earlier call wrote.
    #[test]
    fn an_overflow_splices_the_users_stale_error_message() {
        use crate::interpose::EomReason;

        let port = "test_overflow_stale_message";
        spawn_fill_port(port, 100, EomReason::CNT);
        let mut rec = read_rec(port, ASYN_FMT_ASCII, 80, 0);
        rec.device_user
            .set_error_message(ErrorMessage::format(format_args!("earlier failure")));
        rec.process().unwrap();
        assert_eq!(rec.errs, "Overflow nread 40 earlier failure");
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

    /// C's overflow test is a plain length compare (`nbytesTransferred >=
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
        // NOWT > OMAX sends OMAX bytes, the count C clamps NOWT to (:1499).
        rec.omax = 3;
        rec.nowt = 10;
        assert_eq!(rec.octet_output_buffer(), vec![b'\\', b'r', 0x00]);
    }

    /// R18-84. C sends from an `omax`-sized allocation in every mode: `optr`
    /// for Binary and `pasynRecPvt->outbuff` for ASCII and Hybrid, both
    /// `callocMustSucceed(pasynRec->omax, ...)` (asynRecord.c:295-300), with
    /// `outptr` set to one of them at `:1489/:1495/:1501`. Only Binary has a
    /// count to clamp (`nowt > omax`, `:1499`), so a Hybrid or ASCII write used
    /// to put more than OMAX bytes on the wire — the device saw a payload
    /// longer than the buffer the operator sized for it.
    #[test]
    fn every_output_mode_is_bounded_by_omax() {
        let mut rec = AsynRecord::default();
        rec.omax = 4;

        // Hybrid: BOUT escape-translated, then cut at OMAX.
        rec.ofmt = ASYN_FMT_HYBRID;
        rec.bout = b"abcdefghij".to_vec();
        assert_eq!(
            rec.octet_output_buffer(),
            b"abcd".to_vec(),
            "Hybrid must not exceed OMAX"
        );

        // ASCII: AOUT escape-translated, then cut at OMAX. The translation
        // shortens `\\r\\n` to two bytes, so the bound is measured on the
        // translated payload, exactly as C's `outbuff` bounds it.
        rec.ofmt = ASYN_FMT_ASCII;
        rec.aout = "ab\\r\\ncdef".to_string();
        assert_eq!(
            rec.octet_output_buffer(),
            vec![b'a', b'b', 0x0D, 0x0A],
            "ASCII must not exceed OMAX"
        );

        // Binary already clamped NOWT into the field; the same bound applies
        // when NOWT was never clamped (a payload put with no processing yet).
        rec.ofmt = ASYN_FMT_BINARY;
        rec.nowt = 10;
        assert_eq!(rec.octet_output_buffer(), b"abcd".to_vec());
    }

    /// R18-83: C's `put_array_info` (asynRecord.c:983-993) sets NOWT from the
    /// element count of the BOUT put, and NORD from the count of a BINP put —
    /// `dbPut` calls it on every array put (`dbAccess.c:1365-1368`). Writing the
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
        rec.put_field("BOUT", EpicsValue::CharArray(payload.clone().into()))
            .unwrap();

        assert_eq!(rec.nowt, 120, "BOUT put must set NOWT = nNew");
        assert_eq!(
            rec.octet_output_buffer(),
            payload,
            "every byte the client put must reach the device"
        );

        // The same C function serves BINP: its count is NORD.
        rec.put_field("BINP", EpicsValue::CharArray(vec![1, 2, 3].into()))
            .unwrap();
        assert_eq!(rec.nord, 3, "BINP put must set NORD = nNew");

        // A shorter put shrinks the count — nNew is the count that arrived, not
        // a high-water mark.
        rec.put_field("BOUT", EpicsValue::CharArray(vec![9, 9].into()))
            .unwrap();
        assert_eq!(rec.nowt, 2);
        assert_eq!(rec.octet_output_buffer(), vec![9, 9]);
    }

    /// C's BOUT/BINP are the `omax`/`imax`-byte buffers `init_record` callocs
    /// (asynRecord.c:295-298), read through `get_array_info`'s NOWT/NORD
    /// (:966-976). `dbPut` writes only the `nRequest` elements that arrived,
    /// clamped to the buffer (dbAccess.c:1365-1366), and leaves the rest.
    #[test]
    fn bout_binp_read_their_count_from_a_fixed_buffer() {
        let bytes = |rec: &AsynRecord, f: &str| match rec.get_field(f) {
            Some(EpicsValue::CharArray(v)) => v.to_vec(),
            other => panic!("{f}: {other:?}"),
        };
        let mut rec = AsynRecord::default();
        rec.ofmt = ASYN_FMT_BINARY;
        // A fresh record: NOWT = 80 bytes of the zeroed buffer, on the channel
        // and on the wire.
        assert_eq!(bytes(&rec, "BOUT"), vec![0; 80]);
        assert_eq!(rec.octet_output_buffer(), vec![0; 80]);

        rec.put_field("BOUT", EpicsValue::CharArray(b"abcdefghij".to_vec().into()))
            .unwrap();
        rec.put_field("BOUT", EpicsValue::CharArray(b"xyz".to_vec().into()))
            .unwrap();
        assert_eq!(bytes(&rec, "BOUT"), b"xyz");
        // NOWT is a client field of its own: raising it re-exposes the bytes the
        // shorter put left in place.
        rec.put_field("NOWT", EpicsValue::Long(10)).unwrap();
        assert_eq!(bytes(&rec, "BOUT"), b"xyzdefghij");
        assert_eq!(rec.octet_output_buffer(), b"xyzdefghij");
        rec.ofmt = ASYN_FMT_HYBRID;
        assert_eq!(rec.octet_output_buffer(), b"xyzdefghij");

        // A count past the buffer reads the buffer: C's dbGet would read past
        // `optr` (get_array_info does not clamp NOWT), a C defect not copied.
        rec.put_field("NOWT", EpicsValue::Long(200)).unwrap();
        assert_eq!(bytes(&rec, "BOUT").len(), 80);
        // A put longer than the buffer stores, and counts, OMAX elements.
        rec.put_field("BOUT", EpicsValue::CharArray(vec![7; 100].into()))
            .unwrap();
        assert_eq!(rec.nowt, 80);
        assert_eq!(bytes(&rec, "BOUT"), vec![7; 80]);

        // BINP is the same buffer model over IMAX/NORD.
        let mut rec = AsynRecord::default();
        rec.put_field("NORD", EpicsValue::Long(5)).unwrap();
        assert_eq!(bytes(&rec, "BINP"), vec![0; 5]);
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
        // The value is NOWT/NORD bytes (80 and 0 here), so channel count and
        // value count are genuinely decoupled.
        assert_eq!(
            rec.get_field("BOUT"),
            Some(EpicsValue::CharArray(vec![0; 80].into()))
        );
        assert_eq!(
            rec.get_field("BINP"),
            Some(EpicsValue::CharArray(vec![].into()))
        );
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

    /// `asynRecord.dbd` declares `SPC_DBADDR` on BOUT, BINP and the
    /// `DBF_NOACCESS` ERRS (`:227`, `:284`, `:624`); ERRS's capacity is
    /// `ERR_SIZE` (`asynRecord.c:958`).
    #[test]
    fn the_spc_dbaddr_fields_are_bout_binp_and_errs() {
        use epics_base_rs::server::record::Record;

        let rec = AsynRecord::default();
        for desc in rec.field_list() {
            assert_eq!(
                rec.field_is_dbaddr(desc.name),
                ["BOUT", "BINP", "ERRS"].contains(&desc.name),
                "{}",
                desc.name
            );
        }
        assert_eq!(rec.field_native_count("ERRS"), Some(ERR_SIZE as u32));
    }

    /// R8-54: C `performOctetIO` writes the clamped transfer sizes back into
    /// the record — `nowt = omax` (asynRecord.c:1499) and `nrrd = inlen`
    /// (:1513) — and `monitor()` POST_IF_NEWs them (:1020,1022), so the fields
    /// show the *effective* sizes. The port used to clamp only into locals, so
    /// NOWT/NRRD kept the operator's over-large request forever.
    #[test]
    fn nowt_and_nrrd_are_clamped_back_into_the_record() {
        let port = "r8_54_clamp";
        let _log = spawn_phase_log_port(port);
        let on_port = || {
            let mut rec = AsynRecord::default();
            rec.port = port.to_string();
            rec.connect_device().unwrap();
            rec.iface = InterfaceType::Octet as i32;
            rec
        };
        // What the callback writes once it runs.
        let entered = |rec: &mut AsynRecord| {
            let plan = rec.build_io_plan();
            rec.apply_io_outcome(IoOutcome::entered(&plan));
        };

        // Binary output: NOWT > OMAX clamps to OMAX.
        let mut rec = on_port();
        rec.tmod = TransferMode::WriteRead as i32;
        rec.ofmt = ASYN_FMT_BINARY;
        rec.ifmt = ASYN_FMT_BINARY;
        rec.omax = 8;
        rec.nowt = 500;
        rec.imax = 16;
        rec.nrrd = 900;
        rec.build_io_plan();
        assert_eq!((rec.nowt, rec.nrrd), (500, 900), "planning writes nothing");
        entered(&mut rec);
        assert_eq!(rec.nowt, 8, "NOWT clamps back to OMAX");
        assert_eq!(rec.nrrd, 16, "NRRD clamps back to IMAX for a Binary read");

        // ASCII input: the read capacity is sizeof(AINP), not IMAX
        // (asynRecord.c:1505-1506), so NRRD clamps to 40 even with IMAX=80.
        let mut ascii = on_port();
        ascii.tmod = TransferMode::Read as i32;
        ascii.ifmt = ASYN_FMT_ASCII;
        ascii.imax = 80;
        ascii.nrrd = 900;
        entered(&mut ascii);
        assert_eq!(
            ascii.nrrd, AINP_SIZE as i32,
            "ASCII NRRD clamps to AINP_SIZE"
        );

        // A Read-only cycle still clamps NOWT, and a Write-only cycle still
        // clamps NRRD: C runs both clamps before any TMOD test.
        let mut read_only = on_port();
        read_only.tmod = TransferMode::Read as i32;
        read_only.ofmt = ASYN_FMT_BINARY;
        read_only.omax = 4;
        read_only.nowt = 99;
        entered(&mut read_only);
        assert_eq!(read_only.nowt, 4, "TMOD=Read still clamps NOWT");

        // ASCII/Hybrid output is not length-bounded by NOWT in C, so the NOWT
        // clamp is Binary-only (asynRecord.c:1496-1501).
        let mut ascii_out = on_port();
        ascii_out.tmod = TransferMode::Write as i32;
        ascii_out.ofmt = ASYN_FMT_ASCII;
        ascii_out.omax = 4;
        ascii_out.nowt = 99;
        entered(&mut ascii_out);
        assert_eq!(ascii_out.nowt, 99, "ASCII output leaves NOWT alone");

        // A register interface never reaches performOctetIO, so neither field
        // is touched (asynRecord.c:1326-1360).
        let mut i32_rec = on_port();
        i32_rec.iface = InterfaceType::Int32 as i32;
        i32_rec.tmod = TransferMode::WriteRead as i32;
        i32_rec.ofmt = ASYN_FMT_BINARY;
        i32_rec.omax = 8;
        i32_rec.nowt = 500;
        i32_rec.imax = 16;
        i32_rec.nrrd = 900;
        entered(&mut i32_rec);
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
    /// backed by a real [`PortActor`] thread — the record's process request
    /// queues to it exactly as to a production blocking driver.
    fn canblock_int32_entry(value: i32) -> crate::registry::PortEntry {
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::InterfaceMask;
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

        let mut base = PortDriverBase::new(
            "ASYNIO",
            1,
            PortFlags::default(),
            InterfaceMask::INT32,
            InterfaceMask::NONE,
        );
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        base.set_int32_param(val, 0, value).unwrap();

        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(ReadDriver { base }), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("asynio-test-actor".into())
            .spawn(actor.registered())
            .unwrap();

        let mut handle = PortHandle::new(
            tx,
            "ASYNIO".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        handle.set_can_block(true);
        crate::registry::PortEntry { handle }
    }

    /// C clears ERRS only in `process()`'s `stateIdle` arm (asynRecord.c:339);
    /// the `stateNoDevice` arm only reports (:355-357), unchanged text posting
    /// nothing. A steady portless cycle therefore publishes nothing at all.
    #[tokio::test]
    async fn a_steady_portless_cycle_publishes_nothing() {
        use epics_base_rs::server::database::PvDatabase;

        let mut rec = AsynRecord::default();
        let db = PvDatabase::new();
        rec.async_ctx = Some(("AS".into(), db.async_handle()));
        rec.process().unwrap();
        assert!(
            named(rec.take_process_posted_fields())
                .iter()
                .any(|&(name, _)| name == "ERRS")
        );
        assert_eq!(rec.errs, "Not connect to a port");

        rec.process().unwrap();
        let posts: Vec<_> = rec.take_process_posted_fields().collect();
        assert!(
            posts.is_empty(),
            "a cycle whose ERRS text did not move posted {posts:?}"
        );
    }

    /// Boundary: a `can_block` port with a live async context defers `performIO`
    /// off the scan thread (C `process()` `canBlock` branch, asynRecord.c:344-350)
    /// — `process()` returns `AsyncPending` without the I/O result, and the
    /// completion re-entry (C `pact==TRUE`, asynRecord.c:362-363) applies it.
    #[tokio::test]
    async fn nonblocking_canblock_port_defers_then_applies_on_reentry() {
        use epics_base_rs::server::database::PvDatabase;

        let mut rec = AsynRecord::default();
        rec.device_user
            .attach_for_test(canblock_int32_entry(7), rec.addr);
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        rec.resolved_reason = 0;

        // A live database handle whose record set does NOT contain this record:
        // the completion's re-entry token resolves to nothing, so the test
        // drives the completion re-entry itself, isolating the apply path.
        let db = PvDatabase::new();
        rec.async_ctx = Some(("ASYNIO_REC".into(), db.async_handle()));

        // Pass 1: submitted off-thread, parked — no inline I/O result.
        let out = rec.process().unwrap();
        assert_eq!(
            out.result,
            RecordProcessResult::AsyncPending,
            "a can_block port with async context must defer, not run inline"
        );
        assert!(
            rec.io_pending,
            "the queued request is pending until completion"
        );
        assert_eq!(
            rec.process_cell.as_ref().unwrap().request().priority,
            crate::port::QueuePriority::Low,
            "C queues performIO at asynQueuePriorityLow (asynRecord.c:343)"
        );
        assert_eq!(
            rec.i32inp, 0,
            "the scan thread returned before the read value landed"
        );

        // The port thread leaves the outcome in the process request.
        let job = Arc::clone(&rec.process_job);
        let mut filled = false;
        for _ in 0..2000 {
            if job.outcome.lock().is_some() {
                filled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(filled, "the port thread must leave an outcome");

        // Pass 2: completion re-entry applies the read value and finishes.
        let out2 = rec.process().unwrap();
        assert_eq!(out2.result, RecordProcessResult::Complete);
        assert!(!rec.io_pending, "completion re-entry leaves stateIO");
        assert_eq!(rec.i32inp, 7, "the read value is applied on re-entry");
    }

    /// C `special()` queues every put on its own `duplicateAsynUser` copy
    /// (asynRecord.c:531-533), so a put made while an earlier one is still
    /// queued queues too — `connectDevice` relies on it when it queues the
    /// option and EOS readbacks back to back (:1272-1302). The record's pool
    /// grows by one for it and reuses both once they are released.
    #[test]
    fn a_special_put_queues_while_an_earlier_one_is_outstanding() {
        // C `callbackInit`: no build arms the bands for a bare database.
        epics_base_rs::runtime::task::background_init();
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use epics_base_rs::server::database::PvDatabase;
        use std::sync::Barrier;
        use tokio::sync::mpsc;

        struct BlockingReadDriver {
            base: PortDriverBase,
            entered: Arc<Barrier>,
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
                self.entered.wait();
                self.release.wait();
                Ok(1)
            }
        }

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(BlockingReadDriver {
                base: PortDriverBase::new(
                    "ASYNSPQ",
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION | InterfaceMask::INT32,
                    InterfaceMask::NONE,
                ),
                entered: entered.clone(),
                release: release.clone(),
            }),
            rx,
        );
        let link = actor.link();
        std::thread::spawn(actor.registered());
        let mut handle = PortHandle::new(
            tx,
            "ASYNSPQ".into(),
            Arc::new(InterruptManager::new()),
            link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        handle.set_can_block(true);

        let mut rec = AsynRecord::default();
        rec.device_user
            .attach_for_test(crate::registry::PortEntry { handle }, rec.addr);
        let db = PvDatabase::new();
        rec.set_async_context("ASYNSPQ_REC".into(), db.async_handle());
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;

        // The port thread is inside the driver, so nothing queued behind it runs.
        assert_eq!(
            rec.process().unwrap().result,
            RecordProcessResult::AsyncPending
        );
        entered.wait();

        rec.special("BAUD", true, &Default::default()).unwrap();
        rec.special("PRTY", true, &Default::default()).unwrap();
        assert_eq!(rec.errs, "", "the second put queued behind the first");
        assert_eq!(rec.special_cells.len(), 2);
        assert!(rec.special_cells.iter().all(|s| s.cell.is_outstanding()));

        // Both requests run and release their cells while the drainer is held,
        // so their results are still unapplied.
        let lane = Arc::clone(&rec.special_lane);
        let gate = lane.gate.lock();
        release.wait();
        while rec.special_cells.iter().any(|s| s.cell.is_outstanding()) {
            std::thread::yield_now();
        }
        assert!(!rec.specials_settled(), "a result is still to be applied");
        drop(gate);
        while !rec.specials_settled() {
            std::thread::yield_now();
        }
        rec.special("BAUD", true, &Default::default()).unwrap();
        rec.special("PRTY", true, &Default::default()).unwrap();
        assert_eq!(rec.errs, "");
        assert_eq!(rec.special_cells.len(), 2, "released requests are reused");
    }

    /// Boundary: `AQR` (Abort Queue Request) that loses the race to the port
    /// thread is the C `wasQueued==0` / `callbackActive` case. C `special()`
    /// for `AQR` (asynRecord.c:393-408) calls `cancelRequest`; once the request
    /// has been dequeued and its callback is running, `cancelRequest` reports
    /// `wasQueued==0` and waits for the callback (asynManager.c:1647-1661), so
    /// the I/O runs to completion and is reported normally — `AQR` does NOT
    /// raise "I/O request canceled". Here the read is already parked in the
    /// driver (the executor has claimed the token, `Running`) when `AQR` fires,
    /// so the completion re-entry applies the device value, not CANCELED.
    #[tokio::test]
    async fn aqr_after_driver_dequeue_runs_to_completion() {
        use crate::interrupt::InterruptManager;
        use crate::param::ParamType;
        use crate::port::InterfaceMask;
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
        let mut base = PortDriverBase::new(
            "ASYNAQR",
            1,
            PortFlags::default(),
            InterfaceMask::INT32,
            InterfaceMask::NONE,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        let driver = BlockingReadDriver {
            base,
            value: 7,
            entered: entered.clone(),
            release: release.clone(),
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("asynaqr-test-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let mut handle = PortHandle::new(
            tx,
            "ASYNAQR".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        handle.set_can_block(true);
        let entry = crate::registry::PortEntry { handle };

        let mut rec = AsynRecord::default();
        rec.device_user.attach_for_test(entry, rec.addr);
        rec.tmod = TransferMode::Read as i32;
        rec.iface = InterfaceType::Int32 as i32;
        rec.resolved_reason = 0;
        let db = PvDatabase::new();
        rec.async_ctx = Some(("ASYNAQR_REC".into(), db.async_handle()));

        // No request in flight yet: AQR is the C `wasQueued==false` no-op.
        rec.special("AQR", true, &Default::default()).unwrap();
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
        // parked read so the request completes.
        rec.special("AQR", true, &Default::default()).unwrap();
        release.wait();

        let job = Arc::clone(&rec.process_job);
        let mut filled = false;
        for _ in 0..2000 {
            if job.outcome.lock().is_some() {
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
        assert!(!rec.io_pending, "completion re-entry leaves idle");
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
    /// immediately, with no intervening `process()`. C `exceptCallback` →
    /// `monitorStatus` re-posts the changed trace fields under `dbScanLock`
    /// (asynRecord.c:903-917,1102-1117). The mask change is
    /// driven from a non-runtime thread (the iocsh / port-actor case) to
    /// exercise the captured runtime handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trace_change_posts_readback_fields_immediately() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TraceDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );

        // A trace manager with an exception sink so trace changes announce
        // (without it, `exception_manager()` is None and no callback registers).
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Known baseline mask, then register the port for the record to find.
        trace.set_trace_mask(Some(port_name), TraceMask::empty());
        crate::registry::register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        // Build the record, hand it the database handle, and connect it —
        // connecting registers the trace exception callback with the same
        // async_ctx + runtime handle the framework supplies at add_record.
        let db = PvDatabase::new();
        let rec_name = "TRACE_IMM_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.into(), db.async_handle());
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1, "record must connect to the registered port");
        assert_eq!(rec.tmsk, 0, "baseline trace mask is empty");

        db.add_record(rec_name, Box::new(rec)).await.unwrap();

        // Externally change the trace mask from a non-runtime thread. The
        // captured runtime handle must drive the refresh.
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

    /// The trace readbacks of a connected record against the asyn R4-46 `test`
    /// IOC with `echoDriverInit("P",0.05,0,0)`, `camonitor -m v` on `A.TMSK
    /// A.TB0 A.TB4 A.TIOM A.TIB1 A.CNCT` of `record(asyn,"A"){field(PORT,"P")}`,
    /// driven with `caput A.TMSK 17; asynSetTraceMask P 0 1; caput A.TB4 1;
    /// asynSetTraceIOMask P 0 2; caput A.TMSK 1`. The log stream is the same
    /// and the alarm stream empty; connect values cut.
    ///
    /// Every field posts once per step. A put's setter raises an exception whose
    /// `monitorStatus` runs inside `special()` (asynRecord.c:409-448, :911-916),
    /// against `old` as `REMEMBER_STATE` leaves it: the field the client wrote
    /// is not new to it, the other half of the mask/bit pair is, and dbPut posts
    /// the written field after. An iocsh change posts both halves from the
    /// callback alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trace_puts_and_trace_exceptions_post_as_the_oracle_does() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::{TraceIoMask, TraceManager};
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::types::DbFieldType;
        use std::time::Duration;
        use tokio::sync::mpsc;

        const WANT: &str = "\
--
A.TB4 On
A.TMSK 17
--
A.TMSK 1
A.TB4 Off
--
A.TMSK 17
A.TB4 On
--
A.TIOM 2
A.TIB1 On
--
A.TB4 Off
A.TMSK 1
";

        struct TraceDriver(PortDriverBase);
        impl PortDriver for TraceDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        struct Line {
            pv: &'static str,
            stream: Arc<std::sync::Mutex<String>>,
        }
        impl SubscriptionFilter for Line {
            fn name(&self) -> &'static str {
                "line"
            }
            fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
                let value = match event.event.value().as_value() {
                    Some(EpicsValue::Long(v)) => v.to_string(),
                    Some(EpicsValue::Enum(v)) if self.pv == "A.CNCT" => {
                        ["Disconnect", "Connect"][*v as usize].to_string()
                    }
                    Some(EpicsValue::Enum(v)) => ["Off", "On"][*v as usize].to_string(),
                    other => panic!("{}: {other:?}", self.pv),
                };
                let mut stream = self.stream.lock().unwrap();
                stream.push_str(&format!("{} {value}\n", self.pv));
                Some(event)
            }
        }

        let port_name = "test_trace_oracle_as6";
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TraceDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        crate::registry::register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let db = PvDatabase::new();
        db.add_record("A", Box::new(AsynRecord::default()))
            .await
            .unwrap();
        db.put_record_field_from_ca("A", "PORT", EpicsValue::String(port_name.into()))
            .await
            .unwrap();
        let stream: Arc<std::sync::Mutex<String>> = Arc::default();
        let _readers: Vec<_> = {
            let cell = db.get_record("A").unwrap();
            let mut inst = cell.write();
            let rec = inst
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            assert_eq!(rec.cnct, 1, "record must connect to the registered port");
            [
                ("A.TMSK", "TMSK", DbFieldType::Long),
                ("A.TB0", "TB0", DbFieldType::Enum),
                ("A.TB4", "TB4", DbFieldType::Enum),
                ("A.TIOM", "TIOM", DbFieldType::Long),
                ("A.TIB1", "TIB1", DbFieldType::Enum),
                ("A.CNCT", "CNCT", DbFieldType::Enum),
            ]
            .into_iter()
            .enumerate()
            .map(|(sid, (pv, field, ty))| {
                let reader = inst
                    .add_subscriber(field, sid as u32, ty, EventMask::VALUE.bits())
                    .unwrap();
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone(),
                    }),
                ));
                reader
            })
            .collect()
        };

        // A step's posts, then a settle window for any repeat.
        async fn settle(stream: &std::sync::Mutex<String>, lines: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while stream.lock().unwrap().lines().count() < lines {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the step's posts arrive:\n{}",
                    stream.lock().unwrap()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let iocsh = |f: Box<dyn FnOnce(&TraceManager) + Send>| {
            let tm = trace.clone();
            std::thread::spawn(move || f(&tm)).join().unwrap();
        };
        let put = |field: &'static str, value: &'static str| {
            db.put_record_field_from_ca("A", field, EpicsValue::String(value.into()))
        };
        let step = || stream.lock().unwrap().push_str("--\n");

        step();
        put("TMSK", "17").await.unwrap();
        settle(&stream, 3).await;
        step();
        iocsh(Box::new(|tm| {
            tm.set_trace_mask(Some(port_name), TraceMask::ERROR)
        }));
        settle(&stream, 6).await;
        step();
        put("TB4", "1").await.unwrap();
        settle(&stream, 9).await;
        step();
        iocsh(Box::new(|tm| {
            tm.set_trace_io_mask(Some(port_name), TraceIoMask::ESCAPE)
        }));
        settle(&stream, 12).await;
        step();
        put("TMSK", "1").await.unwrap();
        settle(&stream, 15).await;

        assert_eq!(*stream.lock().unwrap(), WANT);
    }

    /// The specials C hands to `asynCallbackSpecial` (asynRecord.c:834-899)
    /// against the asyn R4-46 `test` IOC with `echoDriverInit("P",0.05,1,0)`
    /// — a blocking octet port with no asynOption, not auto-connected —
    /// `camonitor -m v|a|l -S` on `A.ERRS A.CNCT A.IEOS A.OEOS A.BAUD
    /// A.HOSTINFO` of `record(asyn,"A"){field(PORT,"P") field(ADDR,"-1")}`
    /// and of the same record at the default ADDR 0, driven with `caput
    /// A.CNCT 1; sleep; caput A.IEOS \r; caput A.OEOS \n; caput A.BAUD 9600;
    /// caput A.HOSTINFO x; caput A.CNCT 0; sleep; caput A.CNCT 0`. The value
    /// and log streams are the same and the alarm stream is empty.
    ///
    /// `special()` posts ERRS from `resetError`, dbPut posts the field, and the
    /// callback's posts come after both, from the port thread: the "No
    /// asynOption interface" ERRS follows BAUD and HOSTINFO, and `getEos`
    /// reposts IEOS/OEOS escaped. ERRS and the strings post by reference, so
    /// each step keeps only such a field's first post, with no value, as in
    /// `asyn_special_posts_in_c_order`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn special_callbacks_post_after_the_put_as_the_oracle_does() {
        // C `callbackInit`: no build arms the bands for a bare database.
        epics_base_rs::runtime::task::background_init();
        use crate::interfaces::Capability;
        use crate::interpose::eos::EosInterpose;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::types::DbFieldType;
        use std::time::Duration;

        const WANT: &str = "\
--
A.ERRS
A.CNCT Connect
--
--
A.IEOS
--
A.OEOS
--
A.BAUD 9600
A.ERRS
--
A.ERRS
A.HOSTINFO
--
A.ERRS
A.CNCT Disconnect
--
--
A.CNCT Disconnect
";
        const STEPS: [Option<(&str, &str)>; 9] = [
            Some(("CNCT", "1")),
            None,
            Some(("IEOS", "\r")),
            Some(("OEOS", "\n")),
            Some(("BAUD", "9600")),
            Some(("HOSTINFO", "x")),
            Some(("CNCT", "0")),
            None,
            Some(("CNCT", "0")),
        ];
        const BY_REFERENCE: [&str; 4] = ["A.ERRS", "A.IEOS", "A.OEOS", "A.HOSTINFO"];

        struct Echo(PortDriverBase);
        impl PortDriver for Echo {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                vec![
                    Capability::OctetRead,
                    Capability::OctetWrite,
                    Capability::Flush,
                    Capability::Connect,
                ]
            }
        }

        struct Line {
            pv: &'static str,
            stream: Arc<std::sync::Mutex<String>>,
        }
        impl SubscriptionFilter for Line {
            fn name(&self) -> &'static str {
                "line"
            }
            fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
                let value = match (self.pv, event.event.value().as_value()) {
                    (pv, _) if BY_REFERENCE.contains(&pv) => String::new(),
                    ("A.CNCT", Some(EpicsValue::Enum(v))) => {
                        ["Disconnect", "Connect"][*v as usize].to_string()
                    }
                    ("A.BAUD", Some(EpicsValue::Enum(v))) => BAUD_CHOICES[*v as usize].to_string(),
                    (pv, other) => panic!("{pv}: {other:?}"),
                };
                let line = format!("{} {value}", self.pv);
                let mut stream = self.stream.lock().unwrap();
                stream.push_str(line.trim_end());
                stream.push('\n');
                Some(event)
            }
        }

        fn first_by_reference(stream: &str) -> String {
            let mut out = String::new();
            let mut seen = Vec::new();
            for line in stream.lines() {
                if line == "--" {
                    seen.clear();
                } else if BY_REFERENCE.contains(&line) {
                    if seen.contains(&line) {
                        continue;
                    }
                    seen.push(line);
                }
                out.push_str(line);
                out.push('\n');
            }
            out
        }

        // ADDR -1 is as9 and ADDR 0 as8: a single-device port has no device at
        // any ADDR (`locateDevice`, asynManager.c:576), so the Connect queue
        // takes CNCT 1 from either on the disconnected port.
        let no_option = "No asynOption interface";
        let errs = ["", "", "", "", no_option, no_option, "", "", ""];
        for (port_name, addr) in [("as9_echo", -1), ("as8_echo", 0)] {
            oracle(port_name, addr, &STEPS, WANT, &errs, ("\\r", "\\n")).await;
        }
        // as11: an EOS put the gate refuses on the disconnected port. C
        // reports it from `special()` (asynRecord.c:571-578), so ERRS posts
        // ahead of dbPut's post of the field, and no readback follows.
        const REFUSED: &str = "\
--
A.ERRS
A.CNCT Connect
--
--
A.CNCT Disconnect
--
--
A.ERRS
A.IEOS
--
--
A.ERRS
A.OEOS
--
";
        let refused = "port as11_echo not connected";
        oracle(
            "as11_echo",
            0,
            &[
                Some(("CNCT", "1")),
                None,
                Some(("CNCT", "0")),
                None,
                Some(("IEOS", "\\r")),
                None,
                Some(("OEOS", "\\n")),
                None,
            ],
            REFUSED,
            &["", "", "", "", refused, refused, refused, refused],
            ("\\r", "\\n"),
        )
        .await;

        async fn oracle(
            port_name: &'static str,
            addr: i32,
            steps: &[Option<(&'static str, &'static str)>],
            want: &str,
            errs_after: &[&str],
            eos: (&str, &str),
        ) {
            let mut base = PortDriverBase::new(
                port_name,
                1,
                PortFlags {
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::OCTET,
                InterfaceMask::NONE,
            );
            base.install_octet_interpose(Box::new(EosInterpose::default()));
            base.set_connected(false);
            base.init_auto_connect(false);
            let (rt, jh) =
                create_port_runtime_unclaimed(Echo(base), RuntimeConfig::default()).unwrap();
            std::mem::forget(jh);
            register_port(
                port_name,
                rt.port_handle().clone(),
                &crate::registry::open_owner(),
            )
            .unwrap();
            std::mem::forget(rt);

            let db = PvDatabase::new();
            let mut rec = AsynRecord::default();
            rec.addr = addr;
            db.add_record("A", Box::new(rec)).await.unwrap();
            let cell = db.get_record("A").unwrap();
            // The record's special requests have all completed.
            let settled = || async {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let idle = {
                        let mut inst = cell.write();
                        let rec = inst
                            .record
                            .as_any_mut()
                            .and_then(|any| any.downcast_mut::<AsynRecord>())
                            .unwrap();
                        rec.specials_settled()
                    };
                    if idle {
                        return;
                    }
                    assert!(tokio::time::Instant::now() < deadline, "the lane drains");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let field = |name: &str| cell.read().record.get_field(name);
            let text = |name: &str| match field(name) {
                Some(EpicsValue::CharArray(buf)) => {
                    String::from_utf8(buf.split(|b| *b == 0).next().unwrap().to_vec()).unwrap()
                }
                Some(EpicsValue::String(s)) => s.to_string(),
                other => panic!("{name}: {other:?}"),
            };

            db.put_record_field_from_ca("A", "PORT", EpicsValue::String(port_name.into()))
                .await
                .unwrap();
            settled().await;
            // `connectDevice`'s EOS readback, refused on the disconnected port
            // (asynRecord.c:1296-1301).
            assert_eq!(text("ERRS"), CONNECT_READBACK_QUEUE_REFUSED_MSG);

            let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
            let streams: Vec<Arc<std::sync::Mutex<String>>> =
                classes.iter().map(|_| Arc::default()).collect();
            let mut readers = Vec::new();
            {
                let mut inst = cell.write();
                let mut sid = 0;
                for (class, stream) in classes.iter().zip(&streams) {
                    for (pv, field, ty) in [
                        ("A.ERRS", "ERRS", DbFieldType::Char),
                        ("A.CNCT", "CNCT", DbFieldType::Enum),
                        ("A.IEOS", "IEOS", DbFieldType::String),
                        ("A.OEOS", "OEOS", DbFieldType::String),
                        ("A.BAUD", "BAUD", DbFieldType::Enum),
                        ("A.HOSTINFO", "HOSTINFO", DbFieldType::String),
                    ] {
                        readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                        sid += 1;
                        assert!(inst.attach_filter_to_last_subscriber(
                            field,
                            Arc::new(Line {
                                pv,
                                stream: stream.clone(),
                            }),
                        ));
                    }
                }
            }
            let mut errs = Vec::new();
            for step in steps {
                for stream in &streams {
                    stream.lock().unwrap().push_str("--\n");
                }
                if let Some((field, value)) = *step {
                    db.put_record_field_from_ca("A", field, EpicsValue::String(value.into()))
                        .await
                        .unwrap();
                }
                settled().await;
                errs.push(text("ERRS"));
            }
            assert_eq!(errs, errs_after);
            // `getEos` writes the driver's terminator back escaped (:2003-2024).
            assert_eq!((text("IEOS").as_str(), text("OEOS").as_str()), eos);
            for (got, want, class) in [
                (&streams[0], want, "DBE_VALUE"),
                (
                    &streams[1],
                    "--\n".repeat(steps.len()).as_str(),
                    "DBE_ALARM",
                ),
                (&streams[2], want, "DBE_LOG"),
            ] {
                assert_eq!(first_by_reference(&got.lock().unwrap()), want, "{class}");
            }
        }
    }

    /// C runs `setEos` and the `getEos` it falls through to in one
    /// `asynCallbackSpecial` on the port thread (asynRecord.c:851-855), so a
    /// request another client queues while the driver is inside `setInputEos`
    /// runs after the readback, and IEOS shows the record's own terminator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn special_readback_runs_in_the_same_port_callback() {
        // C `callbackInit`: no build arms the bands for a bare database.
        epics_base_rs::runtime::task::background_init();
        use crate::interfaces::Capability;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        use epics_base_rs::server::database::PvDatabase;
        use std::sync::mpsc;
        use std::time::Duration;

        struct Gated {
            base: PortDriverBase,
            ieos: Vec<u8>,
            entered: std::sync::Mutex<mpsc::Sender<()>>,
            proceed: std::sync::Mutex<mpsc::Receiver<()>>,
        }
        impl PortDriver for Gated {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn capabilities(&self) -> Vec<Capability> {
                vec![Capability::OctetRead, Capability::OctetWrite]
            }
            fn set_input_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                if eos == b"\r" {
                    self.entered.lock().unwrap().send(()).unwrap();
                    self.proceed
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
                self.ieos = eos.to_vec();
                Ok(())
            }
            fn get_input_eos(&self, _user: &AsynUser) -> AsynResult<Vec<u8>> {
                Ok(self.ieos.clone())
            }
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        let port_name = "special_one_callback";
        let base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                can_block: true,
                ..PortFlags::default()
            },
            InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        let driver = Gated {
            base,
            ieos: Vec::new(),
            entered: std::sync::Mutex::new(entered_tx),
            proceed: std::sync::Mutex::new(proceed_rx),
        };
        let (rt, jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
        std::mem::forget(jh);
        let handle = rt.port_handle().clone();
        register_port(port_name, handle.clone(), &crate::registry::open_owner()).unwrap();
        std::mem::forget(rt);

        let db = PvDatabase::new();
        db.add_record("A", Box::new(AsynRecord::default()))
            .await
            .unwrap();
        let cell = db.get_record("A").unwrap();
        let settled = || async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let idle = {
                    let mut inst = cell.write();
                    let rec = inst
                        .record
                        .as_any_mut()
                        .and_then(|any| any.downcast_mut::<AsynRecord>())
                        .unwrap();
                    rec.specials_settled()
                };
                if idle {
                    return;
                }
                assert!(tokio::time::Instant::now() < deadline, "the lane drains");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        db.put_record_field_from_ca("A", "PORT", EpicsValue::String(port_name.into()))
            .await
            .unwrap();
        settled().await;

        db.put_record_field_from_ca("A", "IEOS", EpicsValue::String("\\r".into()))
            .await
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let other = handle
            .try_submit(
                RequestOp::SetInputEos { eos: b"X".to_vec() },
                AsynUser::new(0),
            )
            .unwrap();
        proceed_tx.send(()).unwrap();
        settled().await;
        drop(other);
        assert_eq!(
            cell.read().record.get_field("IEOS"),
            Some(EpicsValue::String("\\r".into()))
        );
    }

    /// Boundary: the exception refresh is C `POST_IF_NEW`
    /// (asynRecord.c:210-214,1102-1117) — `monitorStatus` posts a readback
    /// field only when its value differs from the remembered value. An
    /// `asynSetTraceIOMask` exception recomputes ALL trace readback fields, but
    /// only the IO fields changed, so the unchanged `TMSK` must NOT be
    /// re-posted to its monitor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_trace_field_is_not_reposted() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(TraceDriver(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );

        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        // Baseline masks BEFORE connect so the callback seeds its last-posted
        // cache with these values (the C `old` after the connect-path
        // `monitorStatus`): TMSK = ERROR, all IO bits clear.
        trace.set_trace_mask(Some(port_name), TraceMask::ERROR);
        trace.set_trace_io_mask(Some(port_name), TraceIoMask::empty());
        crate::registry::register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
            rec.special("PORT", true, &Default::default()).unwrap();
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
        // refresh. `ASYN_POSTS` orders TMSK before TIOM in one drain, so once
        // the TIOM event arrives a non-deduped TMSK duplicate would already be
        // queued.
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
    /// not `pp(TRUE)` (asynRecord.dbd:622-627), so nothing else posts it: the
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
        // C reports `no_elements = ERR_SIZE` for ERRS from both `cvt_dbaddr`
        // (asynRecord.c:958) and `get_array_info` (`:978`), so the client sees
        // the whole buffer NUL-padded, never just the message (R18-85).
        let mut buf = text.as_bytes().to_vec();
        buf.resize(ERR_SIZE, 0);
        EpicsValue::CharArray(buf.into())
    }

    #[tokio::test]
    async fn every_errs_write_posts_and_an_unchanged_one_does_not() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION,
            InterfaceMask::NONE,
        );
        base.init_connected(false);
        base.init_auto_connect(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(DownDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        crate::registry::register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let db = PvDatabase::new();
        let rec_name = "ERRS_POST_REC";
        db.add_record(rec_name, Box::new(AsynRecord::default()))
            .await
            .unwrap();
        let mut errs_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.ERRS"))
            .await
            .expect("subscribe ERRS");
        let put = |field: &'static str, value: &str| {
            db.put_record_field_from_ca(rec_name, field, EpicsValue::String(value.into()))
        };
        put("PORT", port_name).await.unwrap();
        while errs_sub.try_recv_event().is_ok() {}

        // Boundary 1 — a refused put. C's `special()` writes the gate's own
        // `pasynUser->errorMessage` to ERRS and frees the user (:571-578); the
        // operator's ERRS monitor must fire with it.
        put("BAUD", "9600").await.unwrap();
        assert_eq!(
            errs_sub.recv().await,
            Some(errs_on_the_wire(&format!("port {port_name} not connected"))),
            "a refused put must fire the ERRS monitor with the refusal text"
        );

        let inst = db.get_record(rec_name).unwrap();
        let mut g = inst.write();
        let rec = g
            .record
            .as_any_mut()
            .unwrap()
            .downcast_mut::<AsynRecord>()
            .unwrap();
        assert!(rec.take_cycle_posted_fields().is_empty(), "the put drained");

        // Boundary 2 — the same text again. C guards the post on
        // `strncmp(errs, old.errs)` (:2044): a record retrying a down port must
        // not fire a monitor per retry with text the client already has.
        rec.report_error(
            AsynStatus::Success,
            format_args!("port {port_name} not connected"),
        );
        assert!(
            rec.take_cycle_posted_fields().is_empty(),
            "an unchanged ERRS must not re-post"
        );

        // Boundary 3 — the clear. C `resetError` (:2050-2060) posts the empty
        // string, so the operator's screen loses the stale message.
        rec.reset_error();
        assert!(
            named(rec.take_cycle_posted_fields())
                .iter()
                .any(|&(name, _)| name == "ERRS"),
            "resetError's clear must post"
        );
    }

    /// Two ERRS reports in one cycle settle on the second one.
    ///
    /// C `reportError` posts inside itself, under the record lock
    /// (asynRecord.c:2044-2048), so `special()`'s entry `resetError` and the
    /// diagnostic that follows it reach the client in that order and the field
    /// ends holding the diagnostic. Here the post is deferred, and while it
    /// carried its own text the two deferrals raced — and `post_fields` *stores*
    /// what it publishes, so the clear's `""` landing second wrote the record's
    /// own ERRS back to empty.
    ///
    /// Measured on the IOC against the disconnected `ORACLEASYN` port: three
    /// `caput T:ASYN.BAUD 9600` in a row left `T:ASYN.ERRS` empty once and
    /// `port ORACLEASYN not connected` twice, where C held the text all three
    /// times. The loop here is that measurement: each pass is one refused put
    /// preceded by a non-empty ERRS, which is what makes the cycle post twice.
    /// Every pass must end on the refusal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn errs_settles_on_the_last_report_of_the_cycle() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
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
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                crate::interfaces::octet_transport_capabilities()
            }
        }

        let port_name = "test_errs_post_order";
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION,
            InterfaceMask::NONE,
        );
        base.init_connected(false);
        base.init_auto_connect(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(DownDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let trace = Arc::new(TraceManager::new());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        trace.set_exception_sink(Arc::new(ExceptionManager::new()));
        crate::registry::register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let db = PvDatabase::new();
        let rec_name = "ERRS_ORDER_REC";
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
            rec.special("PORT", true, &Default::default()).unwrap();
        }

        let refused = format!("port {port_name} not connected");
        for pass in 0..20 {
            {
                let inst = db.get_record(rec_name).unwrap();
                let mut g = inst.write();
                let rec = g
                    .record
                    .as_any_mut()
                    .unwrap()
                    .downcast_mut::<AsynRecord>()
                    .unwrap();
                // A non-empty ERRS at entry is what makes `special()`'s
                // `resetError` post at all — the cycle then posts twice.
                rec.report_error(AsynStatus::Success, format_args!("seed"));
                rec.baud = 9600;
                rec.special("BAUD", true, &Default::default()).unwrap();
                assert_eq!(
                    rec.errs.as_str(),
                    refused,
                    "pass {pass}: the gate refused the put"
                );
            }
            // Let every post this cycle armed reach the record.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let inst = db.get_record(rec_name).unwrap();
            let mut g = inst.write();
            let rec = g
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            assert_eq!(
                rec.errs.as_str(),
                refused,
                "pass {pass}: a post applied out of order wrote ERRS back to \
                 '{}' — the cycle's last report is what the field holds",
                rec.errs
            );
        }
    }

    /// [`AsynRecord::mark`]'s boundaries, on CNCT: C posts it from both
    /// `monitorStatus` calls of `connectDevice` (asynRecord.c:1271, :1319), on
    /// either side of the option readback.
    ///
    /// One case per boundary: a mark ahead of the cursor takes the first copy;
    /// one behind it takes the second; the same post twice with nothing between
    /// is one post; a post past both copies takes the last; a take resets.
    #[test]
    fn a_mark_lands_at_or_after_every_post_marked_before_it() {
        let cnct = STATUS_AT.map(|at| at + 19);
        let mut rec = AsynRecord::default();
        rec.mark(cnct);
        rec.mark(cnct);
        assert_eq!(rec.post_marks, 1 << cnct[0], "CNCT twice is one post");
        rec.mark([OPTIONS_AT]);
        rec.mark(cnct);
        assert_eq!(
            rec.post_marks,
            1 << cnct[0] | 1 << OPTIONS_AT | 1 << cnct[1],
            "CNCT after BAUD takes its second copy"
        );
        rec.mark(STATUS_AT);
        assert_eq!(rec.post_marks & 1 << STATUS_AT[1], 1 << STATUS_AT[1]);
        rec.mark(STATUS_AT);
        assert_eq!(
            rec.post_cursor, cnct[1],
            "a post past both copies takes the last"
        );
        let posts: Vec<_> = rec.take_process_posted_fields().collect();
        assert_eq!(posts.len(), 4);
        rec.mark(cnct);
        assert_eq!(rec.post_marks, 1 << cnct[0], "a take resets the cursor");
    }

    /// R14-47: `connectDevice` posts every readback it refreshes.
    ///
    /// C posts from three places inside the attach, and they are what put the new
    /// port's state on the operator's screen without a `process()`:
    /// `monitorStatus` (asynRecord.c:1271 and again at :1319) POST_IF_NEWs the
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
        use crate::port::InterfaceMask;
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
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
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
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        // See `connect_device_reads_the_eos_back_from_the_driver`: an EOS is
        // only settable on a port carrying C's `asynInterposeEos` (R18-71).
        base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        let mut drv = Serialish(base);
        drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(drv), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

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
        db.put_record_field_from_ca(rec_name, "PORT", EpicsValue::String(port_name.into()))
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
            "monitorStatus posts PCNCT (asynRecord.c:1127)"
        );
    }

    /// R18-86. C `monitor()` posts the input buffers with a bare
    /// `db_post_events` on every read cycle (asynRecord.c:1012-1019), above the
    /// `POST_IF_NEW` block that guards the rest. A device that answers the same
    /// string every scan therefore still fires one AINP and one TINP monitor per
    /// scan; with only generic change detection the operator's screen went
    /// silent the moment the reading stopped moving.
    ///
    /// The boundary is the SECOND scan: the first changes AINP from empty and
    /// would post either way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_steady_reading_still_posts_ainp_every_scan() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use crate::user::AsynUser;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::db_access::DbSubscription;
        use std::time::Duration;
        use tokio::sync::mpsc;

        /// A device stuck on one reading — the case the row is about.
        struct SteadyDevice(PortDriverBase);
        impl PortDriver for SteadyDevice {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                let resp = b"12.345";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                crate::interpose::OctetReadResult::ok(n, crate::interpose::EomReason::empty())
            }
        }

        let port_name = "r18_86_steady";
        let base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(SteadyDevice(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(TraceManager::new()),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let db = PvDatabase::new();
        let rec_name = "R18_86_REC";
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
            rec.special("PORT", true, &Default::default()).unwrap();
            rec.iface = InterfaceType::Octet as i32;
            rec.tmod = TransferMode::Read as i32;
            rec.ifmt = ASYN_FMT_ASCII;
        }

        let mut ainp_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.AINP"))
            .await
            .expect("subscribe AINP");

        db.process_record(rec_name).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), ainp_sub.recv())
                .await
                .expect("first scan must post AINP"),
            Some(EpicsValue::String("12.345".into()))
        );

        // The same bytes again: nothing changed, and C posts anyway.
        db.process_record(rec_name).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), ainp_sub.recv())
                .await
                .expect("an unchanged reading must still post AINP"),
            Some(EpicsValue::String("12.345".into()))
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
        use crate::port::InterfaceMask;
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

        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        crate::registry::register_port(port_name, handle.clone(), &crate::registry::open_owner())
            .unwrap();

        let db = PvDatabase::new();
        let rec_name = "EXCEPT_CNCT_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.into(), db.async_handle());
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
        assert_eq!(
            EXCEPT_REFRESH.worker_count(),
            1,
            "the refresh runs on the Medium exception pool"
        );

        // ENBL / AUCT are re-read by the same refresh and are unchanged, so
        // POST_IF_NEW leaves them alone — the record still shows them true.
        let inst = db.get_record(rec_name).unwrap();
        let g = inst.read();
        assert_eq!(g.record.get_field("ENBL"), Some(EpicsValue::Enum(1)));
        assert_eq!(g.record.get_field("AUCT"), Some(EpicsValue::Enum(1)));
    }

    /// The convergence case: an exception refresh queued while the record is
    /// held, the cycle posting CNCT=0 under that hold, then the queued refresh
    /// and a later exception running. A refresh that sampled CNCT when the
    /// exception fired (1) and posted it after the cycle's 0 left a monitor
    /// showing "Connected" on a dead port, against an old value that already
    /// said 0. The refresh samples and posts under the hold, against the
    /// `PvtOld` the cycle advanced, so the monitor sees 0 once and then 1 on
    /// reconnect, nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refresh_queued_behind_the_cycle_posts_no_stale_connect_state() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use crate::trace::TraceManager;
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::db_access::DbSubscription;
        use epics_base_rs::server::recgbl::EventMask;
        use std::time::Duration;
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

        let port_name = "test_except_converges";
        let exceptions = Arc::new(ExceptionManager::new());
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(exceptions.clone());
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        crate::registry::register_port(port_name, handle.clone(), &crate::registry::open_owner())
            .unwrap();

        let db = PvDatabase::new();
        let rec_name = "EXCEPT_CONVERGES_REC";
        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.set_async_context(rec_name.into(), db.async_handle());
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1, "the port starts connected");
        db.add_record(rec_name, Box::new(rec)).await.unwrap();
        let mut cnct_sub = DbSubscription::subscribe(&db, &format!("{rec_name}.CNCT"))
            .await
            .expect("subscribe CNCT");
        let on_thread = |f: Box<dyn FnOnce() + Send>| std::thread::spawn(f).join().unwrap();

        let inst = db.get_record(rec_name).unwrap();
        {
            // The cycle holds the record.
            let mut held = inst.write();
            // A foreign exception while the port is up: its refresh queues
            // behind the hold. The pause is a stale sampler's window.
            let (tm, pn) = (trace.clone(), port_name.to_string());
            on_thread(Box::new(move || {
                tm.set_trace_mask(Some(&pn), TraceMask::ERROR | TraceMask::FLOW)
            }));
            std::thread::sleep(Duration::from_millis(100));

            // The port goes down inside the cycle, which refreshes and posts 0
            // under its hold, as `raise_exception` does for its own setter.
            let own_raise = {
                let rec = held
                    .record
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<AsynRecord>())
                    .unwrap();
                Arc::clone(&rec.own_raise)
            };
            own_raise.store(RAISE_OPEN, Ordering::Release);
            let h = handle.clone();
            on_thread(Box::new(move || h.disconnect_blocking().unwrap()));
            own_raise.store(RAISE_IDLE, Ordering::Release);
            let rec = held
                .record
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<AsynRecord>())
                .unwrap();
            rec.monitor_status();
            assert_eq!(rec.cnct, 0);
            let _ = held.record.take_cycle_posted_fields();
            held.notify_field("CNCT", EventMask::VALUE | EventMask::LOG);
        }
        assert_eq!(cnct_sub.recv().await, Some(EpicsValue::Enum(0)));

        // The queued refresh runs now and finds nothing new.
        let stale = tokio::time::timeout(Duration::from_millis(300), cnct_sub.recv()).await;
        assert!(
            stale.is_err(),
            "the queued refresh re-posted CNCT: {stale:?}"
        );

        // The next exception, port still down: nothing new either.
        let (tm, pn) = (trace.clone(), port_name.to_string());
        on_thread(Box::new(move || {
            tm.set_trace_mask(Some(&pn), TraceMask::ERROR)
        }));
        let stale = tokio::time::timeout(Duration::from_millis(300), cnct_sub.recv()).await;
        assert!(
            stale.is_err(),
            "a later exception re-posted CNCT: {stale:?}"
        );

        // Reconnect: the monitor converges on the port's state.
        let h = handle.clone();
        on_thread(Box::new(move || h.connect_blocking().unwrap()));
        assert_eq!(cnct_sub.recv().await, Some(EpicsValue::Enum(1)));
        assert_eq!(
            inst.read().record.get_field("CNCT"),
            Some(EpicsValue::Enum(1))
        );
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
        use crate::port::InterfaceMask;
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

        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle.clone(), &crate::registry::open_owner()).unwrap();

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

    /// The record's own setter runs C's `exceptCallback` once, inside
    /// `special()` (asynRecord.c:911-916); the registered callback leaves it
    /// alone. On a record with no database handle a second run shows as
    /// `status_dirty`, which would refresh again on the next `process()`.
    /// One case per announcer: a trace setter (the setter's own thread), an
    /// actor setter (the port actor, before its reply), and a foreign
    /// exception, which still raises the flag.
    #[test]
    fn an_own_setter_refreshes_the_status_once() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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

        let port_name = "test_own_setter_refreshes_once";
        let exceptions = Arc::new(ExceptionManager::new());
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(exceptions.clone());
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);
        rec.status_dirty.store(false, Ordering::Release);

        rec.tmsk = (TraceMask::ERROR | TraceMask::FLOW).bits() as i32;
        rec.special("TMSK", true, &Default::default()).unwrap();
        assert_eq!(rec.tb4, 1, "the refresh in special() ran");
        assert!(!rec.status_dirty.load(Ordering::Acquire), "trace setter");

        rec.enbl = 0;
        rec.special("ENBL", true, &Default::default()).unwrap();
        // One round trip: the actor drains what it owes before it serves this.
        let _ = rec
            .device_user
            .device()
            .unwrap()
            .handle
            .call_param_callbacks_blocking(0);
        assert!(!rec.status_dirty.load(Ordering::Acquire), "actor setter");
        rec.enbl = 1;
        rec.special("ENBL", true, &Default::default()).unwrap();

        std::thread::spawn({
            let trace = trace.clone();
            move || trace.set_trace_mask(Some(port_name), TraceMask::ERROR)
        })
        .join()
        .unwrap();
        assert!(
            rec.status_dirty.load(Ordering::Acquire),
            "a foreign exception still reaches the callback"
        );
    }

    /// C TMSK is an int (asynRecord.c:410), so a driver-defined bit such as
    /// twincat's ASYN_TRACE_INFO 0x40 reaches the port and reads back.
    #[test]
    fn a_trace_mask_write_keeps_every_bit() {
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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

        let port_name = "test_tmsk_driver_bit";
        let trace = Arc::new(TraceManager::new());
        let base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);

        rec.tmsk = 0x41;
        rec.special("TMSK", true, &Default::default()).unwrap();
        assert_eq!(trace.get_trace_mask(Some(port_name)).bits(), 0x41);
        rec.read_trace_state();
        assert_eq!(rec.tmsk, 0x41);
        assert_eq!(rec.tb0, 1);

        // TIOM and TINM are ints too (asynManager.c:2828, :2870).
        rec.tiom = 0x9;
        rec.special("TIOM", true, &Default::default()).unwrap();
        rec.tinm = 0x11;
        rec.special("TINM", true, &Default::default()).unwrap();
        let snap = trace.snapshot(Some(port_name), None);
        assert_eq!(snap.io_mask.bits(), 0x9);
        assert_eq!(snap.info_mask.bits(), 0x11);
        rec.read_trace_state();
        assert_eq!((rec.tiom, rec.tinm), (0x9, 0x11));
    }

    /// C `connectDevice`'s `bad:` tail removes the exception callback
    /// (asynRecord.c:1313) before it unbinds, so an exception on the port the
    /// record just left never reaches it. Through `connect_device` the
    /// callback is already gone (:1154 removes it first), so this drives the
    /// tail on a bound record, as a failure after the add would reach it.
    #[test]
    fn the_bad_tail_stops_exceptions_from_the_old_port() {
        use crate::exception::ExceptionManager;
        use crate::interrupt::InterruptManager;
        use crate::port::InterfaceMask;
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

        let port_name = "test_bad_tail_drops_exceptions";
        let exceptions = Arc::new(ExceptionManager::new());
        let trace = Arc::new(TraceManager::new());
        trace.set_exception_sink(exceptions.clone());
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::NONE,
        );
        base.bind_exception_sink(exceptions.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(PlainDriver(base)), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            port_name.into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            trace.clone(),
        );
        register_port(port_name, handle, &crate::registry::open_owner()).unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.tmod = TransferMode::NoIo as i32;
        let _ = rec.connect_device();
        assert_eq!(rec.cnct, 1);
        let foreign_exception = |mask| {
            let trace = trace.clone();
            std::thread::spawn(move || trace.set_trace_mask(Some(port_name), mask))
                .join()
                .unwrap()
        };
        rec.status_dirty.store(false, Ordering::Release);
        foreign_exception(TraceMask::ERROR | TraceMask::FLOW);
        assert!(
            rec.status_dirty.load(Ordering::Acquire),
            "control: a bound record hears the port"
        );

        let _ = rec.connect_device_bad(crate::manager::DeviceError);
        rec.status_dirty.store(false, Ordering::Release);
        foreign_exception(TraceMask::ERROR);
        assert!(
            !rec.status_dirty.load(Ordering::Acquire),
            "an exception on the old port reached the record after the bad tail"
        );
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
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        let vxi_name = "r10_55_vxi11";
        let (vxi_rt, _vxi_jh) = create_port_runtime_unclaimed(
            DrvVxi11Port::configure(vxi_name, "192.0.2.1", 0, "", "gpib0", 0, true).unwrap(),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            vxi_name,
            vxi_rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = vxi_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.gpibiv, 1, "asynGpib (C :1232-1236)");
        assert_eq!(rec.i32iv, 1, "asynInt32, registered by asynGpib (C :140)");
        assert_eq!(rec.octetiv, 1, "asynOctet");
        assert_eq!(rec.optioniv, 1, "vxi11 registers asynOption (:1777)");

        let prologix_name = "r10_55_prologix";
        let (p_rt, _p_jh) = create_port_runtime_unclaimed(
            DrvAsynPrologixPort::new(prologix_name, "192.0.2.1:1234", true).unwrap(),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            prologix_name,
            p_rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = prologix_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.gpibiv, 1, "asynGpib (C :1232-1236)");
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
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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
        let (rt, _jh) = create_port_runtime_unclaimed(
            OctetTransport(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
        rec.errs = Errs::EMPTY;
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
    /// copies `timeout` (asynManager.c:1227), which the record set to TMOT
    /// (asynRecord.c:818). `setOption` then gets that user (:1758-1832) and an RFC
    /// 2217 negotiation runs under it (asynInterposeCom.c:488,508). The port's
    /// option path carried no asynUser at all, so the record's TMOT never reached
    /// the driver and the negotiation used a private 2 s.
    #[test]
    fn an_option_put_reaches_the_driver_with_the_records_tmot() {
        use crate::interfaces::{Capability, octet_transport_capabilities};
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        use std::sync::Mutex as StdMutex;
        use std::time::Duration;

        static SEEN: StdMutex<Option<Option<Duration>>> = StdMutex::new(None);

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
        let (rt, _jh) = create_port_runtime_unclaimed(
            OptionSpy(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();

        rec.tmot = 3.5;
        rec.lbaud = 19200;
        rec.special("LBAUD", true, &Default::default()).unwrap();

        assert_eq!(
            SEEN.lock().unwrap().take(),
            Some(Some(Duration::from_millis(3500))),
            "the driver's setOption runs under the record's TMOT"
        );
    }

    /// R9-53, the interfaces with no IFACE menu entry: `setOption` and `setEos`
    /// take the same refusal from the same owner. C `setOption` on `!optioniv`
    /// (asynRecord.c:1767-1771) and `setEos` on `!octetiv` (:1957-1961) report
    /// "No asyn<X> interface" and raise COMM_ALARM/MAJOR_ALARM without touching
    /// the driver.
    #[test]
    fn option_and_eos_are_refused_on_a_port_that_lacks_the_interface() {
        use crate::interfaces::Capability;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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
        let (rt, _jh) = create_port_runtime_unclaimed(
            OctetNoOption(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.optioniv, 0, "no asynOption interface");
        assert_eq!(rec.octetiv, 1);

        rec.lbaud = 9600;
        rec.special("LBAUD", true, &Default::default()).unwrap();
        assert_eq!(rec.errs, "No asynOption interface");
        let mut c = CommonFields::default();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);

        // asynOctet is implemented, so the EOS put reaches the driver.
        rec.errs = Errs::EMPTY;
        rec.ieos = "\\n".to_string();
        rec.special("IEOS", true, &Default::default()).unwrap();
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

        let rec = AsynRecord::default();
        let plan = rec.build_io_plan();
        assert_eq!(io_user(&plan).queue_timeout(), Some(QUEUE_TIMEOUT));
        assert_eq!(flush_user(&plan).queue_timeout(), Some(QUEUE_TIMEOUT));
        assert_eq!(
            rec.special_user(OptionQueue::Normal).user().queue_timeout(),
            Some(QUEUE_TIMEOUT)
        );

        // A special shares the process read's Low band (asynRecord.c:343,
        // :564), so neither overtakes the other; only HOSTINFO and the option
        // readback go at Connect (:568, :1281).
        assert_eq!(
            rec.special_user(OptionQueue::Normal).user().priority,
            crate::port::QueuePriority::Low
        );
        assert_eq!(
            rec.special_user(OptionQueue::EvenIfNotConnected)
                .user()
                .priority,
            crate::port::QueuePriority::Connect
        );

        // Negative control: a user built anywhere else (device support, iocsh,
        // a driver's own request) arms no timer — C's `queueRequest(..., 0.0)`.
        assert_eq!(AsynUser::default().queue_timeout(), None);
        assert_eq!(AsynUser::new(3).with_addr(1).queue_timeout(), None);
    }

    /// R10-49. C `queueTimeoutCallbackProcess` (asynRecord.c:919-927): the
    /// process request that never ran reports "process queueRequest timeout",
    /// raises STATE_ALARM/MAJOR_ALARM and forces the record's completion. It is
    /// **not** an I/O result: no transfer happened, so no NAWT/NORD/EOMR is
    /// published.
    #[test]
    fn a_process_queue_timeout_reports_the_c_text_and_state_major() {
        let job = ProcessJob::default();
        job.unrun(Unrun::TimedOut);
        let out = job.outcome.lock().take().unwrap();
        assert_eq!(out.errs.as_deref(), Some("process queueRequest timeout"));
        assert_eq!(
            out.alarm,
            Some((alarm_status::STATE_ALARM, AlarmSeverity::Major))
        );
        assert_eq!(out.nawt, None, "nothing was written — nothing to publish");
    }

    /// Negative control for the queue timeout above: an ordinary I/O failure is
    /// a *result* — the request ran, the device did not answer. It reports the
    /// driver's text with the read-error severity.
    #[test]
    fn an_io_timeout_is_not_a_queue_timeout() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::WriteRead as i32;
        let plan = rec.build_io_plan();

        let mut out = IoOutcome::default();
        record_phase_result(
            &plan,
            &mut out,
            IoPhase::Read,
            Err(crate::error::AsynError::Status {
                status: AsynStatus::Timeout,
                message: "no response".into(),
            }),
        );
        let errs = out.errs.unwrap();
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
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct BaudTextPort(PortDriverBase, &'static str);
        impl PortDriver for BaudTextPort {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn get_option(
                &self,
                _user: &crate::user::AsynUser,
                key: &str,
            ) -> crate::error::AsynResult<String> {
                let key = option_key(key);
                if key == "baud" {
                    Ok(self.1.to_string())
                } else {
                    Err(crate::error::AsynError::OptionNotFound(key))
                }
            }
        }

        let (rt, jh) = create_port_runtime_unclaimed(
            BaudTextPort(
                PortDriverBase::new(
                    port_name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION
                        | InterfaceMask::INT32
                        | InterfaceMask::UINT32_DIGITAL
                        | InterfaceMask::FLOAT64
                        | InterfaceMask::OCTET
                        | InterfaceMask::DRV_USER,
                    InterfaceMask::NONE,
                ),
                baud_text,
            ),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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

        let (rt, jh) = create_port_runtime_unclaimed(
            OctetTransport(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();
        std::mem::forget(rt);
    }

    /// Register a parameter-library port (C `asynPortDriver`: it registers
    /// asynDrvUser and can resolve a drvInfo string).
    fn register_param_port(port_name: &'static str, param: &str) {
        use crate::param::ParamType;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct ParamDriver(PortDriverBase);
        impl PortDriver for ParamDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::OPTION
                | InterfaceMask::INT32
                | InterfaceMask::UINT32_DIGITAL
                | InterfaceMask::FLOAT64
                | InterfaceMask::OCTET
                | InterfaceMask::DRV_USER,
            InterfaceMask::NONE,
        );
        // Reason 0 is taken by a filler param so the resolved reason is non-zero
        // and cannot be confused with the forced-zero case.
        base.params
            .create_param("FILLER", ParamType::Int32)
            .unwrap();
        base.params.create_param(param, ParamType::Int32).unwrap();
        let (rt, jh) = create_port_runtime_unclaimed(ParamDriver(base), RuntimeConfig::default())
            .expect("the port runtime thread must start");
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
    /// port the queue gate refuses it (asynManager.c:1549-1554) and the fields
    /// stay blank, while the option readback — which does carry the waiver
    /// (:1277-1280) — still fills in.
    #[test]
    fn connect_device_reads_the_eos_back_from_the_driver() {
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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
            let mut base = PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            );
            base.init_auto_connect(false);
            base.init_connected(connected);
            // A port that accepts an EOS at all is one C configured with
            // `processEosIn/Out` set, so `asynOctetBase::initialize` installed
            // `asynInterposeEos` above the driver (asynOctetBase.c:170-172).
            // Without that layer the driver's own `setInputEos` is NULL and
            // C answers "not implemented" (R18-71).
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
            // The driver already holds an EOS — st.cmd's `asynOctetSetInputEos`.
            let mut drv = EosPort(base);
            drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
            drv.set_output_eos(&AsynUser::default(), b"\n").unwrap();
            let (rt, jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default())
                .expect("the port runtime thread must start");
            std::mem::forget(jh);
            register_port(
                port_name,
                rt.port_handle().clone(),
                &crate::registry::open_owner(),
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
    /// `isAutoConnect` (:1085-1089), CNCT from `isConnected` (:1090-1094). The
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
            // (asynRecord.c:571-578 — that is R14-46's boundary, asserted in
            // `a_gate_refused_request_reports_the_refusal_and_runs_no_callback`).
            // What this test is about is the *other* side of that line: a request
            // that DID run must end in the tail.
            let handle = rec.device_user.device().unwrap().handle.clone();
            handle.set_auto_connect(-1, false).unwrap();

            // Now the operator puts the field. Whatever the arm does with it, C's
            // callback ends in `monitorStatus`.
            rec.special(field, true, &Default::default()).unwrap();

            assert_eq!(
                rec.auct, 0,
                "{field}: monitorStatus re-imports AUCT from isAutoConnect \
                 (asynRecord.c:1085-1089) at the tail of every arm (:897)"
            );
        }
    }

    /// An IEOS/OEOS put on a port without asynOctet clears both fields, as the
    /// asyn R4-46 `testAsynPortDriver` IOC does for `record(asyn,"A")
    /// {field(PORT,"testAPD")}` driven with `caput A.IEOS \r; caput A.OEOS \n`:
    /// ERRS reads "No asynOctet interface" and IEOS/OEOS read back empty after
    /// each put. C `setEos` returns on `!octetiv` (asynRecord.c:1957-1961) but
    /// the callback falls through to `getEos`, whose `goto post` writes both
    /// blank buffers over the fields (:1997-1998, :2016-2024).
    #[test]
    fn an_eos_put_without_asyn_octet_reads_back_blank() {
        use crate::interfaces::Capability;
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct Int32Only(PortDriverBase);
        impl PortDriver for Int32Only {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                vec![Capability::Int32Read, Capability::Int32Write]
            }
        }
        let port_name = "as10_no_octet";
        let base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            InterfaceMask::INT32,
            InterfaceMask::NONE,
        );
        let (rt, jh) =
            create_port_runtime_unclaimed(Int32Only(base), RuntimeConfig::default()).unwrap();
        std::mem::forget(jh);
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();
        std::mem::forget(rt);

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.octetiv, 0);
        for field in ["IEOS", "OEOS"] {
            rec.put_field(field, EpicsValue::String("\r".into()))
                .unwrap();
            rec.special(field, true, &Default::default()).unwrap();
            assert_eq!(rec.errs, "No asynOctet interface", "{field}");
            assert_eq!((rec.ieos.as_str(), rec.oeos.as_str()), ("", ""), "{field}");
        }
    }

    /// The `callbackConnect` arm's driver diagnostics are C's literals
    /// (asynRecord.c:869-884): "asynCallbackSpecial callbackConnect
    /// connect: %s" / "… disconnect: %s" over `pasynUser->errorMessage` — the
    /// driver's text alone. Its "isConnected error" (:860-864) needs an
    /// `isConnected` that fails inside the callback, which neither C's manager
    /// nor the actor's answer can.
    #[test]
    fn cnct_reports_the_c_texts() {
        use crate::port::InterfaceMask;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct Refuses(PortDriverBase);
        impl PortDriver for Refuses {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "no route".into(),
                })
            }
            fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "stuck".into(),
                })
            }
        }
        let start = |port_name: &'static str, connected: bool| {
            let mut base = PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION
                    | InterfaceMask::INT32
                    | InterfaceMask::UINT32_DIGITAL
                    | InterfaceMask::FLOAT64
                    | InterfaceMask::OCTET
                    | InterfaceMask::DRV_USER,
                InterfaceMask::NONE,
            );
            base.set_connected(connected);
            base.init_auto_connect(false);
            let (rt, jh) =
                create_port_runtime_unclaimed(Refuses(base), RuntimeConfig::default()).unwrap();
            std::mem::forget(jh);
            register_port(
                port_name,
                rt.port_handle().clone(),
                &crate::registry::open_owner(),
            )
            .unwrap();
            let mut rec = AsynRecord::default();
            rec.port = port_name.to_string();
            // A port-level user, so the Connect queue takes the request on a
            // disconnected port.
            rec.addr = -1;
            let _ = rec.connect_device();
            (rec, rt)
        };
        let cnct = |rec: &mut AsynRecord, value: &str| {
            rec.put_field("CNCT", EpicsValue::String(value.into()))
                .unwrap();
            rec.special("CNCT", true, &Default::default()).unwrap();
        };

        let (mut rec, _rt) = start("cnct_texts_connect", false);
        cnct(&mut rec, "1");
        assert_eq!(
            rec.errs,
            "asynCallbackSpecial callbackConnect connect: no route"
        );

        let (mut rec, _rt) = start("cnct_texts_disconnect", true);
        cnct(&mut rec, "0");
        assert_eq!(
            rec.errs,
            "asynCallbackSpecial callbackConnect disconnect: stuck"
        );
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
        rec.special("REASON", true, &Default::default()).unwrap();
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

    /// The drvUser `drvUserCreate` hangs on the record's `pasynUser`
    /// (asynRecord.c:1250) rides its transfers and its special requests.
    #[test]
    fn a_drv_user_from_drvinfo_rides_the_io_and_special_users() {
        use crate::param::ParamType;
        use crate::port::{DrvUserInfo, DrvUserRequest, PortDriver, PortDriverBase, PortFlags};
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        struct ProgDriver(PortDriverBase);
        impl PortDriver for ProgDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
                Ok(self
                    .0
                    .drv_user_create(&DrvUserRequest::new("START_", req.addr))?
                    .with_drv_user(Arc::new(req.drv_info.clone())))
            }
        }
        let port_name = "drv_user_rides";
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            crate::port::InterfaceMask::OPTION
                | crate::port::InterfaceMask::INT32
                | crate::port::InterfaceMask::UINT32_DIGITAL
                | crate::port::InterfaceMask::FLOAT64
                | crate::port::InterfaceMask::OCTET
                | crate::port::InterfaceMask::DRV_USER,
            crate::port::InterfaceMask::OCTET,
        );
        base.params
            .create_param("START_", ParamType::Int32)
            .unwrap();
        let (rt, _jh) =
            create_port_runtime_unclaimed(ProgDriver(base), RuntimeConfig::default()).unwrap();
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
        )
        .unwrap();

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "START_PROG1".to_string();
        rec.connect_device().unwrap();
        let prog = |user: &AsynUser| {
            user.drv_user
                .as_ref()
                .and_then(|d| d.downcast_ref::<String>())
                .cloned()
        };
        let plan = rec.build_io_plan();
        assert_eq!(prog(&io_user(&plan)).as_deref(), Some("START_PROG1"));
        assert_eq!(prog(&flush_user(&plan)).as_deref(), Some("START_PROG1"));
        let special = rec.special_user(OptionQueue::Normal).user();
        assert_eq!(prog(&special).as_deref(), Some("START_PROG1"));

        // And the record's interrupt node, as C's keeps the same pasynUser.
        let _wakeups = rec.io_intr_scan().take_receiver().unwrap();
        rec.set_io_intr_scan(true);
        let clients = rt.port_handle().interrupts().subscribed_clients();
        let [client] = clients.as_slice() else {
            panic!("{clients:?}");
        };
        let tag = client.filter.drv_user.as_ref();
        let tag = tag.and_then(|d| d.downcast_ref::<String>());
        assert_eq!(tag.map(String::as_str), Some("START_PROG1"));
    }

    /// R18-89. C assigns the reason only on success:
    /// `if (status == asynSuccess) pasynRec->reason = pasynUser->reason; else
    /// reportError(...)` (asynRecord.c:1248-1257). The failure arm touches
    /// neither `pasynRec->reason` nor `pasynUser->reason`, so a bad DRVINFO put
    /// leaves the record pointed where it already was.
    ///
    /// Zeroing the resolved reason instead split the two: REASON kept reading
    /// back what the operator set while every read and write went to parameter 0
    /// — the record operating on the wrong parameter with no sign of it beyond
    /// the ERRS text.
    #[test]
    fn a_failed_drv_user_create_leaves_the_resolved_reason_alone() {
        let port_name = "r18_89_create_fail";
        register_param_port(port_name, "GAIN");

        let mut rec = AsynRecord::default();
        rec.port = port_name.to_string();
        rec.drvinfo = "GAIN".to_string();
        rec.connect_device().unwrap();
        assert_eq!((rec.reason, rec.resolved_reason), (1, 1));

        // The operator mistypes DRVINFO. C reports it and changes nothing else.
        rec.drvinfo = "GAIM".to_string();
        rec.connect_device().unwrap();
        assert_eq!(rec.errs, "Error in asynDrvUser->create()");
        assert_eq!(
            rec.resolved_reason, 1,
            "the I/O must not be redirected to parameter 0"
        );
        assert_eq!(
            rec.reason, 1,
            "and REASON must not disagree with what the I/O uses"
        );
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
        // R18-89: a failed create assigns neither reason, so the previous
        // resolution stands.
        assert_eq!(rec.resolved_reason, 1);
        assert_eq!(rec.reason, 1);

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
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (rt, _jh) = create_port_runtime_unclaimed(
            IoIntrDriver {
                base: PortDriverBase::new(
                    name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::OPTION
                        | InterfaceMask::INT32
                        | InterfaceMask::UINT32_DIGITAL
                        | InterfaceMask::FLOAT64
                        | InterfaceMask::OCTET
                        | InterfaceMask::DRV_USER,
                    InterfaceMask::INT32
                        | InterfaceMask::UINT32_DIGITAL
                        | InterfaceMask::FLOAT64
                        | InterfaceMask::OCTET,
                ),
                reads: reads.clone(),
            },
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
    /// I32INP and sets `gotValue` (:746-749), and `process` skips **all** I/O for
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
    /// 717-719,739-741,760-762,781-783). The FIRST unprocessed value is kept, not
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
        // fails to resolve, which unbinds the record (C `stateNoDevice`).
        rec.port = "R12_46_NO_SUCH_PORT".to_string();
        let _ = rec.connect_device();
        assert!(
            rec.device_user.device().is_none(),
            "the record now has no port"
        );

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
        fire_interrupt(
            &rt,
            0,
            ParamValue::Octet(b"ab\n"[..].into()),
            RegIface::Octet,
            0,
        );
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
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

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
        let (rt, _jh) = create_port_runtime_unclaimed(
            OctetOnly(PortDriverBase::new(
                port_name,
                1,
                PortFlags::default(),
                InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::NONE,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        register_port(
            port_name,
            rt.port_handle().clone(),
            &crate::registry::open_owner(),
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
            rec.special(field, true, &Default::default()).unwrap();
            assert!(
                !scan.is_active(),
                "{field}: C forces SCAN back to Passive, cancelling the registration"
            );

            // PCNCT=0 detached the port (C :522-526); reconnect so the read below
            // has a device. The reconnect must not re-arm the scan by itself.
            if field == "PCNCT" {
                rec.pcnct = 1;
                rec.special("PCNCT", true, &Default::default()).unwrap();
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

    /// C `cancelIOInterruptScan` puts SCAN with `dbPutField` inside
    /// `special()` (asynRecord.c:794-806), so the REASON / IFACE / UI32MASK /
    /// PCNCT put that called it returns with SCAN already Passive, and the post
    /// is made by the putting thread. The SCAN post's thread is the boundary: a
    /// put handed to the callback pool posts from a pool worker after the put
    /// returned.
    #[tokio::test]
    async fn a_put_that_cancels_io_intr_leaves_scan_passive_before_it_returns() {
        use epics_base_rs::server::database::PvDatabase;
        use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::types::DbFieldType;

        struct Poster(Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>);
        impl SubscriptionFilter for Poster {
            fn name(&self) -> &'static str {
                "poster"
            }
            fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
                self.0.lock().unwrap().push(std::thread::current().id());
                Some(event)
            }
        }

        let (_rt, _reads) = io_intr_port("cancel_scan_inline");
        let db = PvDatabase::new();
        db.add_record("A", Box::new(AsynRecord::default()))
            .await
            .unwrap();
        for (field, value) in [
            ("PORT", "cancel_scan_inline"),
            ("IFACE", "asynInt32"),
            ("TMOD", "Read"),
            ("SCAN", "I/O Intr"),
        ] {
            db.put_record_field_from_ca("A", field, EpicsValue::String(value.into()))
                .await
                .unwrap();
        }
        let cell = db.get_record("A").unwrap();
        let posts: Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>> = Arc::default();
        let _reader = {
            let mut inst = cell.write();
            let rec = inst
                .record
                .as_any_mut()
                .unwrap()
                .downcast_mut::<AsynRecord>()
                .unwrap();
            assert!(rec.io_intr.is_active(), "SCAN=I/O Intr armed the record");
            let reader = inst
                .add_subscriber("SCAN", 0, DbFieldType::Enum, EventMask::VALUE.bits())
                .unwrap();
            assert!(inst.attach_filter_to_last_subscriber("SCAN", Arc::new(Poster(posts.clone()))));
            reader
        };

        db.put_record_field_from_ca("A", "REASON", EpicsValue::String("3".into()))
            .await
            .unwrap();
        let scan = cell.read().common.scan;
        let posted = posts.lock().unwrap().clone();
        assert_eq!(
            scan,
            ScanType::Passive,
            "SCAN is Passive when the put returns"
        );
        assert_eq!(
            posted,
            [std::thread::current().id()],
            "one SCAN post, made by the putting thread"
        );
    }
}
