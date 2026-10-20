// RTEMS-EXEC-MODEL-ALLOW(44): checked, not waived — all 44 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1485/1485). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource};
use epics_base_rs::server::device_support::{
    DeviceAlarm, DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf, PropertyPost,
};
use epics_base_rs::server::device_support::{ReadStart, WriteStart};
use epics_base_rs::server::record::{CommonFields, DevSup, Record, ScanType};
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::c_parse::{long_to_int, strtol_errno, strtoul_errno, ulong_to_uint};
use epics_libcom_rs::cfmt::CFloat;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::InterfaceType;
use crate::interrupt::{
    ENUM_ENTRY_BYTES, InterruptFilter, InterruptReceiver, InterruptSubscription, MAX_ENUM_STATES,
    clamp_enum_table,
};
use crate::port::{DrvUserRequest, QueuePriority};
use crate::port_handle::{AsyncCompletionHandle, PortHandle};
use crate::request::{
    CellBody, OctetRead, PortCalls, ReadEntry, RequestCell, RequestOp, RequestResult, Unrun,
};
use crate::user::{AsynUser, DEFAULT_TIMEOUT, timeout_from_secs};

/// C `MAX_STRING_SIZE`, a stringin/stringout VAL.
const MAX_STRING_SIZE: usize = 40;

/// Parsed `@asyn(portName, addr, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynLink {
    pub port_name: String,
    pub addr: i32,
    /// `None` is the link's negative timeout — C's "wait forever"
    /// (`@asyn(PORT,0,-1)`, asynEpicsUtils.c:125). See
    /// [`crate::user::AsynUser::timeout`].
    pub timeout: Option<Duration>,
    pub drv_info: String,
}

/// The outcome of a store that landed a value.
///
/// `wrote_val` is `store_read_value`'s answer: `true` when the adapter
/// produced the finished VAL (C `return 2`), `false` when it left the raw
/// reading in RVAL for the record to convert (C `return 0`). Either way the
/// record is DEFINED, because every asyn dset pairs its success arm with
/// `pr->udf = 0` before returning — `processAi:850-852`, `processBi:1161-1164`,
/// `processMbbi:1268-1270`, `processLi:1057-1060` (devAsynInt32.c). Kept here
/// rather than on `DeviceReadOutcome` so the framework's constructors never
/// grow a form that infers UDF from the value half.
/// An output transfer's alarm before its dset's default applies: the
/// result's own, or the status it failed with.
fn write_alarm(ran: &AsynResult<RequestResult>) -> Result<(u16, u16), AsynStatus> {
    match ran {
        Ok(r) => Ok((r.alarm_status, r.alarm_severity)),
        Err(e) => Err(e.status()),
    }
}

fn stored(wrote_val: bool) -> DeviceReadOutcome {
    if wrote_val {
        DeviceReadOutcome::computed(DeviceUdf::Defined)
    } else {
        DeviceReadOutcome::converted(DeviceUdf::Defined)
    }
}

/// C `strtol(s, &endp, 0)` into the link's `int addr`, `errno` tested
/// (asynEpicsUtils.c:113-115, :185-187): `long` range, then the `int`
/// assignment's truncation.
fn strtol_i32(s: &str) -> Option<(i32, &str)> {
    let (value, end) = strtol_errno(s, 0)?;
    Some((long_to_int(value), &s[end..]))
}

/// C `strtoul(s, &endp, 0)` into the link's `epicsUInt32 mask`: a leading `-`
/// negates modulo 2^64 (so `-8` is `0xFFFFFFF8` after the cast — the bit
/// pattern asynInt32 reads as a signed nbits, see [`Int32Mask::from_nbits`]).
fn strtoul_u32(s: &str) -> Option<(u32, &str)> {
    let (value, end) = strtoul_errno(s, 0)?;
    Some((ulong_to_uint(value), &s[end..]))
}

/// C `skipWhite` (asynEpicsUtils.c:48-52): whitespace, and commas too where
/// `comma_ok` — so `p 0 1`, `p,0,1`, `p, 0 ,1` and `p 0,` all separate alike.
fn skip_white(s: &str, comma_ok: bool) -> &str {
    s.trim_start_matches(|c| epics_base_rs::runtime::stdlib::c_isspace(c) || (comma_ok && c == ','))
}

/// The fields C's `parseLink` / `parseLinkMask` fill.
struct InstIo {
    port_name: String,
    addr: i32,
    mask: u32,
    timeout: Option<Duration>,
    drv_info: String,
}

const PARSE_LINK_ERROR: &str = "invalid INST_IO Must be asyn(<port> <addr> <timeout>)userParams";
const PARSE_LINK_MASK_ERROR: &str =
    "invalid INST_IO Must be asynMask(<port> <addr> <mask> <timeout>)userParams";

/// C `parseLink`'s INST_IO arm (asynEpicsUtils.c:91-145) and, with `mask`,
/// `parseLinkMask` (:152-213): one scanner, the mask field sitting between
/// addr and timeout. `Err` is C's `pasynUser->errorMessage`.
fn parse_inst_io(link: &str, mask: bool) -> Result<InstIo, &'static str> {
    let (opening, error, not_inst_io) = if mask {
        ("asynMask(", PARSE_LINK_MASK_ERROR, "Link must be INST_IO")
    } else {
        ("asyn(", PARSE_LINK_ERROR, "Link must be INST_IO or VME_IO")
    };
    // dbParseLink trims the field and an INST_IO link is the text after `@`.
    let Some(text) = link.trim().strip_prefix('@') else {
        return Err(not_inst_io);
    };
    let at = text.find(opening).ok_or(error)?;
    let p = skip_white(&text[at + opening.len()..], false);
    let port_len = p
        .find(|c| epics_base_rs::runtime::stdlib::c_isspace(c) || c == ',' || c == ')')
        .ok_or(error)?;
    let port_name = p[..port_len].to_string();

    let mut p = skip_white(&p[port_len..], true);
    let mut addr = 0;
    let mut bits = 0;
    let mut timeout = Some(DEFAULT_TIMEOUT);
    'fields: {
        if p.is_empty() || (mask && p.starts_with(')')) {
            return Err(error);
        }
        if p.starts_with(')') {
            break 'fields;
        }
        let rest;
        (addr, rest) = strtol_i32(p).ok_or(error)?;
        p = skip_white(rest, true);
        if mask {
            if p.is_empty() || p.starts_with(')') {
                return Err(error);
            }
            let rest;
            (bits, rest) = strtoul_u32(p).ok_or(error)?;
            p = skip_white(rest, true);
        }
        if p.is_empty() {
            return Err(error);
        }
        if p.starts_with(')') {
            break 'fields;
        }
        let read = epics_base_rs::runtime::stdlib::strtod(p.as_bytes());
        if read.erange {
            return Err(error);
        }
        // A negative value is C's "wait forever"; `timeout_from_secs` owns
        // that mapping (DRV-42).
        timeout = timeout_from_secs(read.value);
        // No comma here: C closes the timeout with `skipWhite(pnext, 0)`.
        p = skip_white(&p[read.len..], false);
        if !p.starts_with(')') {
            return Err(error);
        }
    }
    Ok(InstIo {
        port_name,
        addr,
        mask: bits,
        timeout,
        drv_info: skip_white(&p[1..], false).to_string(),
    })
}

/// Parse an `@asyn(portName addr timeout)drvInfo` link as C `parseLink` does:
/// addr and timeout are optional, and whitespace and commas both separate.
pub fn parse_asyn_link(s: &str) -> Result<AsynLink, AsynError> {
    let io = parse_inst_io(s, false).map_err(|e| AsynError::InvalidLinkSyntax(e.into()))?;
    Ok(AsynLink {
        port_name: io.port_name,
        addr: io.addr,
        timeout: io.timeout,
        drv_info: io.drv_info,
    })
}

/// Parsed `@asynMask(portName, addr, mask, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynMaskLink {
    pub port_name: String,
    pub addr: i32,
    pub mask: u32,
    /// `None` is C's "wait forever" — see [`AsynLink::timeout`].
    pub timeout: Option<Duration>,
    pub drv_info: String,
}

/// Parse an `@asynMask(portName addr mask timeout)drvInfo` link as C
/// `parseLinkMask` does: addr and mask are required, the timeout optional.
pub fn parse_asyn_mask_link(s: &str) -> Result<AsynMaskLink, AsynError> {
    let io = parse_inst_io(s, true).map_err(|e| AsynError::InvalidLinkSyntax(e.into()))?;
    Ok(AsynMaskLink {
        port_name: io.port_name,
        addr: io.addr,
        mask: io.mask,
        timeout: io.timeout,
        drv_info: io.drv_info,
    })
}

/// asynInt32 `@asynMask` bit-count (nbits) configuration.
///
/// For the **asynInt32** interface the third `@asynMask` argument is a
/// signed bit COUNT, not a raw bitmask (C devAsynInt32.c:233-248):
/// a negative count selects *bipolar* handling (sign-extend on read),
/// a positive count *unipolar* (plain low-bit mask). The derived `mask`
/// keeps the low `|nbits|` bits; `sign_bit` is the top of that field;
/// `device_low`/`device_high` are the raw device range used for the
/// LINEAR ESLO/EOFF slope (convertAi:444-451), and they take precedence
/// over the driver's `getBounds` (initAi:824-827).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Int32Mask {
    mask: u32,
    sign_bit: u32,
    bipolar: bool,
    device_low: i32,
    device_high: i32,
}

impl Int32Mask {
    /// Derive from the signed nbits value (the reinterpreted `@asynMask`
    /// 3rd arg). Returns `None` for `nbits == 0` — C leaves `mask = 0`,
    /// so the read/interrupt masking blocks are skipped entirely.
    fn from_nbits(nbits_signed: i32) -> Option<Self> {
        if nbits_signed == 0 {
            return None;
        }
        let bipolar = nbits_signed < 0;
        // Clamp to 32 so the shifts below stay defined (C relies on the
        // platform `int` width; asynInt32 is 32-bit).
        let nbits = nbits_signed.unsigned_abs().min(32);
        // C `~(~0 << nbits)`: the low `nbits` bits set. `<< 32` is UB in
        // C and panics in Rust, so the full-width case is taken directly.
        let mask = if nbits >= 32 {
            u32::MAX
        } else {
            !(!0u32 << nbits)
        };
        let sign_bit = 1u32 << (nbits - 1);
        let (device_low, device_high) = if bipolar {
            // C: deviceLow = ~(mask/2)+1 (= -(mask/2)); deviceHigh = mask/2.
            let half = (mask / 2) as i32;
            (-half, half)
        } else {
            // C: deviceLow = 0; deviceHigh = mask.
            (0, mask as i32)
        };
        Some(Self {
            mask,
            sign_bit,
            bipolar,
            device_low,
            device_high,
        })
    }

    /// C `processCallbackInput` / `interruptCallbackInput` mask +
    /// sign-extend (devAsynInt32.c:485-488 / 537-540): keep the low
    /// `nbits`, then for bipolar fields with the sign bit set, extend the
    /// sign into the high bits.
    fn apply(self, value: i32) -> i32 {
        let mut v = (value as u32) & self.mask;
        if self.bipolar && (v & self.sign_bit) != 0 {
            v |= !self.mask;
        }
        v as i32
    }

    /// C applies the mask to `pPvt->result.value` as soon as the read returns
    /// (devAsynInt32.c:485-488, initAo :951-953), so everything after it —
    /// the trace line, the record store — sees the masked value.
    fn apply_to_read(mask: Option<Self>, result: &mut RequestResult) {
        if let (Some(m), Some(v)) = (mask, result.int_val.as_mut()) {
            *v = m.apply(*v);
        }
    }
}

/// Adapter bridging an asyn-rs PortDriver to epics-base-rs DeviceSupport.
pub struct AsynDeviceSupport {
    handle: PortHandle,
    /// The record's one request on a blocking port — see [`DeviceJob`].
    device_job: Arc<DeviceJob>,
    request_cell: Option<Arc<RequestCell>>,
    addr: i32,
    /// `None` is the link's "wait forever" — see [`AsynLink::timeout`].
    timeout: Option<Duration>,
    drv_info: String,
    reason: usize,
    reason_set: bool,
    iface_type: String,
    /// Typed interface (resolved from `iface_type` string at construction).
    iface: Option<InterfaceType>,
    /// Bit mask for UInt32Digital read/write. Default: 0xFFFFFFFF.
    mask: u32,
    /// asynInt32 `@asynMask` nbits config (mask + sign-extend + device
    /// bounds), derived when the interface is `asynInt32` and a non-zero
    /// bit count was given. `None` for every other case (no masking).
    int32_mask: Option<Int32Mask>,
    /// C `pPvt->deviceLow`/`deviceHigh`: the raw range `convertAi`/`convertAo`
    /// scale over, fixed at init and (0, 0) until then.
    device_bounds: (i64, i64),
    last_alarm_status: u16,
    last_alarm_severity: u16,
    last_ts: Option<epics_base_rs::types::WallTime>,
    /// C `pPvt->previousQueueRequestStatus`, the last refused `queueRequest`'s.
    queue_status: AsynStatus,
    record_name: String,
    /// C `pPvt->isIOIntrScan`: set by `get_ioint_info`'s cmd 0 and cleared by
    /// its cmd 1 (devAsynInt32.c:383/:399), read by the average callback on
    /// the driver's thread. Seeded from the loaded SCAN.
    io_intr_scan: Arc<std::sync::atomic::AtomicBool>,
    /// Maximum number of array elements for array read operations.
    /// Default: 307200 (enough for 640x480 images).
    max_array_elements: usize,
    /// C devAsynOctet `valSize`, the record buffer a read lands in: SIZV for
    /// lsi/lso/printf, NELM for waveform, `sizeof(val)` = 40 for
    /// stringin/stringout (devAsynOctet.c `initCommon` callers). A read keeps
    /// at most `valSize - 1` bytes of it.
    octet_val_size: usize,
    /// The `asynOctet` read request size: `octet_val_size`, cut to
    /// `octet_len_cap`.
    octet_max_size: usize,
    /// Per-record octet length cap from `drv_user_create` (C
    /// `modbusDrvUser_t.len` parsed from a `TYPE=N` drvInfo). `Some(n)` caps
    /// `octet_max_size` to `min(octet_max_size, n)` at init, mirroring C
    /// `getStringLen` (drvModbusAsyn.cpp:2367-2377); `None` leaves the buffer
    /// length alone. Resolved once at bind.
    octet_len_cap: Option<usize>,
    /// The drvUser `drv_user_create` returned, which every request of this
    /// record carries (C's one `pasynUser` per record).
    drv_user: Option<crate::port::DrvUser>,
    /// `Some` ⟹ this is `asynOctetCmdResponse`: an escape-translated literal
    /// command (C `initCmdBuffer`, devAsynOctet.c) written before each read so
    /// the reply lands in VAL. `read_op` emits `OctetWriteRead` instead of a
    /// plain `OctetRead` when present. `None` for ordinary `asynOctetRead`.
    octet_cmd: Option<Vec<u8>>,
    /// If true, read back the current driver value during init (for output records).
    initial_readback: bool,
    /// C's `isOutput`: the record writes, so its interrupt client is
    /// `interruptCallbackOutput`, and on devAsynOctet it reads
    /// `info(asyn:INITIAL_READBACK)` (devAsynOctet.c:357).
    output: bool,
    /// `info(asyn:READBACK, "1")` flag, asyn upstream PRs #60 / #208.
    /// When set on an output record, the adapter activates the
    /// driver-callback path even when `SCAN != "I/O Intr"` so the
    /// record reprocesses on every external value change. Wired
    /// through `io_intr_receiver` together with the existing IoIntr
    /// scan path.
    asyn_readback: bool,
    /// If true, this is a write-only device support (e.g. asynOctetWrite).
    /// read() returns no-op to avoid overwriting the record's native value type.
    write_only: bool,
    /// asynOctetWriteBinary: write the record's full NORD bytes with NO NUL-trim
    /// (C `callbackWfWriteBinary`, devAsynOctet.c:1086-1091, `writeIt(bptr, nord)`),
    /// versus asynOctetWrite which trims at the first NUL (`my_strnlen`). Only
    /// meaningful together with `write_only`.
    octet_binary: bool,
    /// `Some` ⟹ averaging device support (`asynInt32Average` /
    /// `asynFloat64Average`, both ai-only). The always-on interrupt callback
    /// accumulates samples into the [`SumAverager`](crate::interfaces::average::SumAverager);
    /// the periodic record process drains the arithmetic mean — C
    /// `interruptCallbackAverage` (devAsynInt32.c:647-710 /
    /// devAsynFloat64.c:464-521) + `processAiAverage` (devAsynInt32.c:887-928 /
    /// devAsynFloat64.c:708-755). Both of C's models are ported: the
    /// periodic-SCAN mean of every sample since the last process, and the
    /// `SCAN="I/O Intr"` SVAL-decimation model that emits one mean per SVAL
    /// samples (devAsynInt32.c:673-702, see `AverageState::num_to_average`).
    average: Option<Arc<AverageState>>,
    /// RAII handle for the averaging synchronous interrupt callback — dropping
    /// unregisters it. Distinct from `interrupt_sub` (the value path into
    /// the ring): averaging accumulates every sample itself and pushes one
    /// decimated mean (C `registerInterruptUser`).
    average_callback_sub: Option<crate::interrupt::SyncCallbackSubscription>,
    /// `Some` ⟹ time-series device support (`asynInt32TimeSeries` /
    /// `asynFloat64TimeSeries` / `asynInt64TimeSeries`, all waveform-only). The
    /// interrupt callback, registered while `BUSY`, appends each driver sample
    /// into the array; the record process (RARM-driven + the completion callback)
    /// commits the buffer to VAL/NORD/BUSY — C `devAsynXXXTimeSeries.h`
    /// `interruptCallback` + `process`. `None` for every other DTYP.
    time_series: Option<Arc<TimeSeriesState>>,
    /// RAII handle for the time-series synchronous accumulating callback,
    /// held while BUSY — dropping unregisters it (the time-series support
    /// needs every sample, so a synchronous callback, not a mailbox).
    time_series_sub: Option<crate::interrupt::SyncCallbackSubscription>,
    /// The time-series record's reprocess wakeup, sent at buffer-full.
    time_series_wake: Option<tokio::sync::mpsc::Sender<()>>,
    /// RAII handle for the `SCAN="I/O Intr"` synchronous interrupt callback —
    /// dropping unregisters it. Synchronous like the averaging and time-series
    /// callbacks: the C ring (`interruptCallbackInput`, devAsynInt32.c:564-576)
    /// holds every callback the record has not processed yet, so the values
    /// go straight from the driver's callback into `interrupt_fifo`. A
    /// latest-only mailbox in between kept one value per record-process and
    /// dropped the rest silently, which is not the ring's overflow count.
    interrupt_sub: Option<crate::interrupt::SyncCallbackSubscription>,
    /// Per-record ring buffer of interrupt values, FIFO-ordered. The
    /// I/O Intr forwarding task pushes; `read()` pops the oldest
    /// entry. C parity: `devAsynInt32.c::ringBuffer` (DEFAULT 10,
    /// configurable via `info("asyn:FIFO")`). Overflow policy:
    /// drop-oldest + `overflows++`; the record-process wakeup is
    /// **only** sent on a fresh-entry add (not on overflow
    /// overwrite) so the dbScan queue does not flood.
    interrupt_fifo: Arc<std::sync::Mutex<InterruptFifo>>,
    /// `info("asyn:FIFO")` as C's `atoi` reads it; `None` keeps the dset's
    /// default ([`Self::settle_ring`]).
    fifo_tag: Option<usize>,
    /// The `interruptAccept` the ring/average callbacks drop on.
    gate: Arc<RecordGate>,
    /// The C dset whose lines this record prints; settled at `init`, once
    /// the record type is known.
    dset: Dset,
    /// C `devAsynInt32.c::newOutputCallbackValue` (asyn devEpics, the
    /// `devPvt` flag). Set when a driver-callback readback cycle is armed
    /// (`arm_readback_callback`, mirrors `outputCallbackCallback` setting
    /// the flag to 1 before `dbProcess`) and cleared the moment the record
    /// actually reaches its read stage (`read()` clears it, mirroring
    /// `processBo` clearing the flag). If the cycle never reaches `read()`
    /// — the PACT entry guard bailed because a put / FLNK cycle still owns
    /// the record — the flag survives and `reconcile_readback_callback`
    /// discards the stale ring entry (C `outputCallbackCallback`'s fallback
    /// `getCallbackValue`), so every output callback consumes exactly one
    /// ring entry (1 wakeup == 1 pop). Without it a readback that races the
    /// record's own put leaves the FIFO desynced and the final driver value
    /// (e.g. AD `Acquire` returning to 0) is never popped.
    output_callback_pending: bool,
    /// The record's `scanIoRequest`, rung by [`InterruptFifo::ring`], until
    /// `io_intr_receiver` takes it. One channel for the record's life, as C's
    /// `ioScanPvt` is, so a client `get_ioint_info` registers again reaches
    /// the pump iocInit spawned.
    scan_io_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    /// True once the `asynFloat64` ai path has written VAL at least once.
    /// The SMOO filter primes on the first read (C `processAi` skips
    /// smoothing while `pr->udf`, devAsynFloat64.c:599); this is the
    /// adapter-side `!udf` for that path, where the adapter is the sole
    /// VAL writer so "have I produced a value yet" is exactly C's prime
    /// signal — and it does not depend on the framework's UDF lifecycle.
    smoo_primed: bool,
    /// Enum state-field family captured at init when the record exposes
    /// ZRST/ZNAM and the port has asynEnum (C's
    /// `findInterface(asynEnumType) && maxEnums>0` gate). `Some` arms the
    /// runtime re-propagation callback ([`Self::property_post_receiver`]).
    enum_shape: Option<EnumRecordShape>,
    /// The driver enum table applied at init, `None` when the read failed —
    /// the diff seed for the
    /// runtime callback so a value-only change (enum index moves, choices
    /// unchanged) fires no DBE_PROPERTY post (C's asynEnum callback only
    /// fires on `doCallbacksEnum`, never on an int32 value callback).
    enum_choices: Option<Arc<[crate::param::EnumEntry]>>,
    /// RAII subscription for the runtime enum-table interrupt — dropping
    /// unsubscribes. Distinct from `interrupt_sub` (the value I/O Intr
    /// subscription); C registers the asynEnum callback separately.
    enum_interrupt_sub: Option<InterruptSubscription>,
    /// The tables `init`'s asynEnum registration receives, until
    /// `property_post_receiver` takes them.
    enum_intr_rx: Option<InterruptReceiver>,
}

/// Shared state for an averaging (`asynInt32Average` / `asynFloat64Average`)
/// record. The always-on interrupt callback (registered in `init`, by
/// [`AsynDeviceSupport::subscribe_average`]) pushes each driver sample into
/// `averager` and stashes the sample's driver alarm in `last_alarm`; the
/// periodic record-process `read()` drains the arithmetic mean and applies
/// the alarm. Mirrors C `devPvt.sum`/`numAverage` (devAsynInt32.c:98-99) plus
/// the `alarmStatus`/`alarmSeverity` captured in `interruptCallbackAverage`
/// (devAsynInt32.c:705-707). Behind an `Arc` because the interrupt callback
/// runs off the record-process thread, exactly like `interrupt_fifo`.
struct AverageState {
    averager: crate::interfaces::average::SumAverager,
    /// The two status channels C `processAiAverage` reads, held under one lock
    /// so the drain reads them atomically against the accumulating callback.
    status: std::sync::Mutex<AverageStatus>,
    /// Mode 1 (SCAN="I/O Intr") decimation count: C `numToAverage =
    /// (int)(pai->sval + 0.5)`, floored at 1 (devAsynInt32.c:674-675). C reads
    /// `pai->sval` live in the callback; the Rust analogue snapshots SVAL from
    /// the record at init and on each process (`refresh_average_decimation_threshold`),
    /// so a runtime SVAL change takes effect at the next process. Default 1.
    /// Unused in Mode 2 (periodic SCAN).
    num_to_average: std::sync::atomic::AtomicI64,
}

/// The two status channels C's averaging device support accumulates across a
/// period. C `interruptCallbackAverage` does
/// `result.status |= auxStatus; result.alarmStatus = alarmStatus;
/// result.alarmSeverity = alarmSeverity` per sample (devAsynInt32.c:705-707,
/// devAsynFloat64.c:516-518); `processAiAverage` then maps `result.status` via
/// `asynStatusToEpicsAlarm` and gates the store on `result.status ==
/// asynSuccess` (devAsynInt32.c:915-927, devAsynFloat64.c:732-754).
#[derive(Default)]
struct AverageStatus {
    /// Last sample's EPICS `(alarm_status, alarm_severity)` (C
    /// `result.alarmStatus`/`alarmSeverity`). On a transport-success period
    /// these pass straight through; on a transport error C's
    /// `asynStatusToEpicsAlarm` fills only the still-`NO_ALARM` fields, so a
    /// sample's own EPICS alarm still wins.
    last_alarm: (u16, u16),
    /// Accumulated asyn transport status (C `result.status |= auxStatus`).
    /// `None` ⟹ every sample this period was `asynSuccess`; `Some(s)` ⟹ a
    /// non-success transport status occurred, which makes the period a
    /// transport error (discard the averaged value, raise the mapped alarm).
    /// Reset each consumed period (C resets `result.status = asynSuccess`).
    aux_error: Option<crate::error::AsynStatus>,
}

impl AverageState {
    fn new() -> Self {
        Self {
            averager: crate::interfaces::average::SumAverager::new(),
            status: std::sync::Mutex::new(AverageStatus::default()),
            num_to_average: std::sync::atomic::AtomicI64::new(1),
        }
    }
}

/// Time-series accumulation buffer, typed to the record's FTVL. Resolved at
/// `init()` from FTVL (which C requires be the signed/unsigned EPICS type of the
/// interface: Int32→LONG/ULONG, Float64→DOUBLE, Int64→INT64/UINT64). The driver
/// sample is appended in the element type so the committed VAL matches FTVL.
enum TsBuf {
    Long(Vec<i32>),
    Int64(Vec<i64>),
    UInt64(Vec<u64>),
    Double(Vec<f64>),
}

impl TsBuf {
    fn len(&self) -> usize {
        match self {
            Self::Long(v) => v.len(),
            Self::Int64(v) => v.len(),
            Self::UInt64(v) => v.len(),
            Self::Double(v) => v.len(),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Long(v) => v.clear(),
            Self::Int64(v) => v.clear(),
            Self::UInt64(v) => v.clear(),
            Self::Double(v) => v.clear(),
        }
    }

    /// Append one driver sample (C `interruptCallback`: `pData[nord] = value`).
    /// The interface and FTVL are paired by construction (validated at init), so
    /// the matching arm always fires; a non-matching ParamValue is ignored.
    fn push(&mut self, value: &crate::param::ParamValue) {
        use crate::param::ParamValue;
        match (self, value) {
            (Self::Long(v), ParamValue::Int32(x)) => v.push(*x),
            (Self::Int64(v), ParamValue::Int64(x)) => v.push(*x),
            (Self::UInt64(v), ParamValue::Int64(x)) => v.push(*x as u64),
            (Self::Double(v), ParamValue::Float64(x)) => v.push(*x),
            _ => {}
        }
    }

    /// C's `(double)value` of one sample.
    fn sample(value: &crate::param::ParamValue) -> f64 {
        use crate::param::ParamValue;
        match value {
            ParamValue::Int32(x) => *x as f64,
            ParamValue::Int64(x) => *x as f64,
            ParamValue::UInt64(x) => *x as f64,
            ParamValue::Float64(x) => *x,
            _ => 0.0,
        }
    }

    /// Snapshot the accumulated samples as the record VAL value. `set_val`/
    /// `put_field("VAL")` then sets `NORD = len` (C `pwf->nord = pPvt->nord`).
    fn to_epics_value(&self) -> EpicsValue {
        match self {
            Self::Long(v) => EpicsValue::LongArray(v.as_slice().into()),
            Self::Int64(v) => EpicsValue::Int64Array(v.as_slice().into()),
            Self::UInt64(v) => EpicsValue::UInt64Array(v.as_slice().into()),
            Self::Double(v) => EpicsValue::DoubleArray(v.as_slice().into()),
        }
    }
}

/// Shared state for a time-series (`asynInt32TimeSeries` /
/// `asynFloat64TimeSeries` / `asynInt64TimeSeries`) waveform record. The
/// accumulating callback (registered by the process while BUSY) appends
/// samples while `busy`; the record process commits them. Mirrors C
/// `devAsynXXXTimeSeries.h`'s `devAsynWfPvt` (busy/nord live here; the array is
/// `buf`, committed to the record VAL each process rather than written into the
/// record buffer from the callback — the asyn-rs callback runs inline in the
/// driver notify and holds no record lock).
struct TimeSeriesState {
    inner: std::sync::Mutex<TimeSeriesInner>,
}

struct TimeSeriesInner {
    /// Acquisition active (C `pPvt->busy`). The callback appends only while set;
    /// the process state machine sets it from RARM and the callback clears it
    /// when the buffer fills.
    busy: bool,
    /// Buffer capacity = record NELM, captured at init.
    nelm: usize,
    /// Accumulated samples, typed to FTVL. `None` only before init has run: an
    /// invalid FTVL kills the record instead (`DeviceInitOutcome::Dead`), so a
    /// record that ever processes has a typed buffer.
    buf: Option<TsBuf>,
    /// Transport status of the most recent erroring sample (C `pPvt->status =
    /// pasynUser->auxStatus`), applied as a READ_ALARM and cleared by the next
    /// process.
    status: crate::error::AsynStatus,
}

impl TimeSeriesState {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(TimeSeriesInner {
                busy: false,
                nelm: 0,
                buf: None,
                status: crate::error::AsynStatus::Success,
            }),
        }
    }
}

/// Cached interrupt value with metadata for alarm/timestamp propagation.
#[derive(Clone)]
struct CachedInterrupt {
    value: crate::param::ParamValue,
    timestamp: SystemTime,
    /// Driver-reported alarm (EPICS alarm status/severity) captured at
    /// callback time. C devAsynInt32.c:561-563 stores rp->alarmStatus /
    /// rp->alarmSeverity in each ring-buffer element so processXxx can
    /// recGblSetSevr them (devAsynInt32.c:843-847). Mirrors the polled
    /// read path's `RequestResult` alarm carrier.
    alarm_status: u16,
    alarm_severity: u16,
    /// asyn transport status carried by the ring element (C
    /// `ringBuffer[].status = pasynUser->auxStatus`, devAsynInt32.c:600). The
    /// read maps it to an alarm and gates the value store on `Success`, exactly
    /// as C `processAi`/`processBo`/array `process()` gate on `result.status`.
    aux_status: crate::error::AsynStatus,
    /// [`crate::interrupt::InterruptValue::octet_counts_nul`] of the fire.
    octet_nul: bool,
}

/// The `interruptAccept` a record's callbacks read: the gate of the database
/// that bound the record (`bind_interrupt_gate`), or the port's own
/// `accepting()` while none has. Both reads are lock- and allocation-free;
/// the callbacks run inline in the driver's notify under a record's hold.
///
/// C's callbacks return before the ring push while the gate is closed
/// (`devAsynInt32.c:545-556`, `:663`; `devAsynOctet.c:526-532`;
/// `devAsynUInt32Digital.c:449-460`; `devAsynXXXArray.cpp:407-410`).
struct RecordGate {
    bound: std::sync::OnceLock<Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>>,
    port: Arc<crate::interrupt::InterruptManager>,
}

impl RecordGate {
    fn open(&self) -> bool {
        match self.bound.get() {
            Some(gate) => gate.is_open(),
            None => self.port.accepting(),
        }
    }
}

/// Per-record store of I/O Intr callbacks: C's `ringBuffer` + `ringSize` +
/// `ringBufferOverflows`, or with `ringSize` 0 the latest-value slot.
struct InterruptFifo {
    store: CallbackStore,
    /// C `ringBufferOverflows`: drop-oldest overwrites since the last pop,
    /// which hands them over in its `Delivery`.
    overflows: u64,
    /// C `asyncProcessingActive`: a queued write holds the record PACT, and
    /// the callbacks it collects are rung for when it ends.
    processing_async: bool,
    /// The record's process requests. A doorbell that carries no count:
    /// [`Self::waiting`] is what the record is owed, and [`Self::ring`] keeps
    /// that many outstanding as far as the channel holds.
    bell: tokio::sync::mpsc::Sender<()>,
    /// Requests rung and not yet answered: the queued ones, and the one the
    /// pump has taken for the pass it is running ([`Self::outstanding`]). A
    /// pop and the end of a pump pass answer it ([`Self::answered`]).
    rung: usize,
}

enum CallbackStore {
    /// C `ringSize > 0`: every callback not yet processed, oldest first.
    Ring {
        entries: std::collections::VecDeque<CachedInterrupt>,
        size: usize,
    },
    /// C `ringSize == 0` (devAsynOctet.c:490-521, devAsynXXXArray.cpp:412-428):
    /// a good callback's value goes straight into the record, and every
    /// callback counts `gotValue` and asks for a process, which reads
    /// whatever the record holds by then.
    Latest {
        /// The last good value, with the last callback's status, alarm and
        /// time (C `result`). Replaced in place: a callback allocates
        /// nothing, the value being an `Arc`.
        last: Option<CachedInterrupt>,
        /// `last.value` came from a good callback: C copies none other.
        good: bool,
        /// The callbacks owed a process: C `gotValue`'s `scanIoRequest`s.
        owed: u32,
        /// The callbacks before `interruptAccept` no process has taken: C
        /// `gotValue`'s other half ([`InterruptFifo::hold`]).
        held: u32,
        /// A pass has run on `last` since it landed. C resets `result.status`
        /// after a failed pass (devAsynOctet.c:821, devAsynXXXArray.cpp:339),
        /// so the slot's later passes succeed, keeping the alarm the first
        /// one mapped into `result` (devAsynOctet.c:806-809).
        spent: bool,
    },
}

/// What a process takes from the store, and whether it writes the value:
/// a ring entry's only if it is good (C `processAi`), the latest slot's
/// whenever one good callback has landed, as C's record already holds it.
struct Delivery {
    entry: CachedInterrupt,
    store: bool,
    /// The ring overflows this pop reports and clears (C `getCallbackValue`).
    overflows: u64,
    /// More than one callback landed since the latest slot's last take: C
    /// `gotValue` after the decrement, the "multiple interrupt callbacks" case.
    backlog: bool,
    /// An earlier pass already reported this latest-slot entry's status.
    spent: bool,
}

/// C `DEFAULT_RING_BUFFER_SIZE` at `devAsynInt32.c:63`.
const DEFAULT_RING_BUFFER_SIZE: usize = 10;

impl CallbackStore {
    fn sized(size: usize) -> Self {
        if size == 0 {
            Self::Latest {
                last: None,
                good: false,
                owed: 0,
                held: 0,
                spent: false,
            }
        } else {
            Self::Ring {
                entries: std::collections::VecDeque::with_capacity(size),
                size,
            }
        }
    }
}

impl InterruptFifo {
    fn new(bell: tokio::sync::mpsc::Sender<()>) -> Self {
        Self {
            store: CallbackStore::sized(DEFAULT_RING_BUFFER_SIZE),
            overflows: 0,
            processing_async: false,
            bell,
            rung: 0,
        }
    }

    /// C `createRingBuffer`: the store `size` asks for, empty.
    fn resize(&mut self, size: usize) {
        self.store = CallbackStore::sized(size);
    }

    fn is_latest(&self) -> bool {
        matches!(self.store, CallbackStore::Latest { .. })
    }

    /// C `ringSize`.
    fn size(&self) -> usize {
        match self.store {
            CallbackStore::Ring { size, .. } => size,
            CallbackStore::Latest { .. } => 0,
        }
    }

    /// The callbacks still owed a process: ring entries, or C `gotValue`.
    fn waiting(&self) -> usize {
        match &self.store {
            CallbackStore::Ring { entries, .. } => entries.len(),
            CallbackStore::Latest { owed, .. } => *owed as usize,
        }
    }

    /// C `getCallbackValue`'s test, or ringSize 0's `gotValue`: the next
    /// process takes a callback's value rather than reading the driver.
    fn unread(&self) -> bool {
        match &self.store {
            CallbackStore::Ring { entries, .. } => !entries.is_empty(),
            CallbackStore::Latest { owed, held, .. } => *owed > 0 || *held > 0,
        }
    }

    /// C's `scanIoRequest` per callback (devAsynInt32.c:574,
    /// devAsynOctet.c:518): one more process request while fewer are
    /// outstanding than callbacks are owed. Called after every callback and every pop, so
    /// a burst past the channel's capacity is still processed once per
    /// callback. Inside a queued write nothing rings until it ends
    /// (`devAsynInt32.c:607-613`).
    fn ring(&mut self) {
        if !self.processing_async
            && self.outstanding() < self.waiting()
            && self.bell.try_send(()).is_ok()
        {
            self.rung += 1;
        }
    }

    /// The requests in the channel.
    fn queued(&self) -> usize {
        self.bell.max_capacity() - self.bell.capacity()
    }

    /// The requests still to process the record: the queued ones and the
    /// one the pump has taken, which C's overflow does not request again
    /// (devAsynInt32.c:565-572). The pump runs one pass at a time, so a
    /// request it dropped without a pass (record off I/O Intr, IOC paused)
    /// counts as that one until the next pop or pass end.
    fn outstanding(&self) -> usize {
        self.rung.min(self.queued() + 1)
    }

    /// The taken request is answered, by a pop or by the end of a pump pass
    /// that popped nothing; only the queued ones are left.
    fn answered(&mut self) {
        self.rung = self.queued();
    }

    /// The bottom of C's `processXxx` (`devAsynInt32.c:1013-1018`): a queued
    /// write's hold ends, and a callback it collected is requested. Any other
    /// cycle rings nothing here: one that popped rang in `pop`, and one that
    /// bailed on PACT (dbAccess.c:537) lost its request. Its entry is rung for
    /// at the next callback or pop; C requested it once (devAsynInt32.c:574,
    /// devAsynOctet.c:518) and stays one process behind for good, not copied.
    fn end_processing(&mut self) {
        if std::mem::take(&mut self.processing_async) {
            self.ring();
        }
    }

    /// devAsynOctet's ringSize-0 callback before `interruptAccept`: C copies
    /// the value (devAsynOctet.c:490-509) but its `scanIoRequest` does nothing
    /// before `scanRun` (dbScan.c:621), so no process is owed for it; the next
    /// process, PINI included, takes the value as C's `gotValue` has it do.
    /// A ring keeps nothing it does not owe.
    fn hold(&mut self, entry: CachedInterrupt) {
        if let CallbackStore::Latest { held, .. } = &mut self.store {
            *held = held.saturating_add(1);
            self.land(entry);
        }
    }

    /// The latest slot takes a callback: C copies a good value into the
    /// record and every status into `result` (devAsynOctet.c:490-510).
    fn land(&mut self, entry: CachedInterrupt) {
        let CallbackStore::Latest {
            last, good, spent, ..
        } = &mut self.store
        else {
            return;
        };
        *spent = false;
        match last {
            Some(last) if entry.aux_status != crate::error::AsynStatus::Success => {
                last.timestamp = entry.timestamp;
                last.alarm_status = entry.alarm_status;
                last.alarm_severity = entry.alarm_severity;
                last.aux_status = entry.aux_status;
            }
            slot => {
                *good = entry.aux_status == crate::error::AsynStatus::Success;
                *slot = Some(entry);
            }
        }
    }

    /// Producer-side: store a callback. Returns `true` when the record
    /// should be asked to process: a fresh ring entry, or any latest-slot
    /// callback; `false` on a ring overflow (no scanIoRequest in C parity).
    fn push_with_overflow(&mut self, entry: CachedInterrupt) -> bool {
        match &mut self.store {
            CallbackStore::Ring { entries, size } => {
                let fresh = entries.len() < *size;
                if !fresh {
                    entries.pop_front();
                    self.overflows += 1;
                }
                entries.push_back(entry);
                fresh
            }
            CallbackStore::Latest { owed, .. } => {
                *owed = owed.saturating_add(1);
                self.land(entry);
                true
            }
        }
    }

    /// Consumer-side: the oldest ring entry, or one `gotValue` of the latest
    /// slot, and a request for the next one still owed. `None` when nothing
    /// is waiting.
    fn pop(&mut self) -> Option<Delivery> {
        let delivery = match &mut self.store {
            CallbackStore::Ring { entries, .. } => entries.pop_front().map(|entry| Delivery {
                store: entry.aux_status == crate::error::AsynStatus::Success,
                entry,
                overflows: std::mem::take(&mut self.overflows),
                backlog: false,
                spent: false,
            }),
            // A take clears the callbacks held before the gate with it. C's
            // `gotValue` keeps them (devAsynOctet.c:506), ahead for good, so
            // its later processes skip their driver read and warn; not copied.
            CallbackStore::Latest {
                last,
                good,
                owed,
                held,
                spent,
            } if *owed > 0 || *held > 0 => {
                let backlog = owed.saturating_add(*held) > 1;
                *owed = owed.saturating_sub(1);
                *held = 0;
                let delivery = last.clone().map(|entry| Delivery {
                    entry,
                    store: *good,
                    overflows: 0,
                    backlog,
                    spent: *spent,
                });
                *spent = true;
                delivery
            }
            CallbackStore::Latest { .. } => None,
        };
        // Counting the taken request past its pop would leave the last owed
        // callback unrung.
        if delivery.is_some() {
            self.answered();
        }
        self.ring();
        delivery
    }

    /// C `outputCallbackCallback` for a callback whose `dbProcess` did not
    /// process: a ring entry is popped and dropped, `gotValue` is left
    /// (devAsynOctet.c:587-589).
    fn discard(&mut self) -> Option<Delivery> {
        match self.store {
            CallbackStore::Ring { .. } => self.pop(),
            CallbackStore::Latest { .. } => None,
        }
    }
}

impl AsynDeviceSupport {
    /// Whether this devAsyn file defers a driver callback that arrives
    /// while a queued write holds the record (`numDeferredOutputCallbacks`):
    /// every one with output callbacks but `devAsynXXXArray`.
    fn defers_output_callbacks(&self) -> bool {
        self.asyn_readback && self.dset != Dset::Array
    }

    /// The bottom of C's `processXxx`: the cycle is over, and a callback
    /// it deferred is requested now (`devAsynInt32.c:1013-1018`).
    fn end_processing(&self) {
        self.interrupt_fifo.lock().unwrap().end_processing();
    }

    /// Create from a [`PortHandle`].
    pub fn from_handle(handle: PortHandle, link: AsynLink, iface_type: &str) -> Self {
        let iface = InterfaceType::from_asyn_name(iface_type);
        let gate = Arc::new(RecordGate {
            bound: std::sync::OnceLock::new(),
            port: handle.interrupts().clone(),
        });
        let (bell, scan_io_rx) = tokio::sync::mpsc::channel(16);
        Self {
            gate,
            dset: Dset::of("", iface_type),
            handle,
            addr: link.addr,
            timeout: link.timeout,
            drv_info: link.drv_info,
            reason: 0,
            reason_set: false,
            iface_type: iface_type.to_string(),
            iface,
            mask: 0xFFFFFFFF,
            int32_mask: None,
            device_bounds: (0, 0),
            max_array_elements: 307200,
            octet_val_size: 256,
            octet_max_size: 256,
            octet_len_cap: None,
            drv_user: None,
            octet_cmd: None,
            last_alarm_status: 0,
            last_alarm_severity: 0,
            last_ts: None,
            queue_status: AsynStatus::Success,
            record_name: String::new(),
            io_intr_scan: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            initial_readback: false,
            output: false,
            asyn_readback: false,
            write_only: false,
            octet_binary: false,
            average: None,
            average_callback_sub: None,
            time_series: None,
            time_series_sub: None,
            time_series_wake: None,
            interrupt_sub: None,
            scan_io_rx: Some(scan_io_rx),
            interrupt_fifo: Arc::new(std::sync::Mutex::new(InterruptFifo::new(bell))),
            fifo_tag: None,
            output_callback_pending: false,
            smoo_primed: false,
            enum_shape: None,
            enum_choices: None,
            enum_interrupt_sub: None,
            enum_intr_rx: None,
            device_job: Arc::new(DeviceJob::default()),
            request_cell: None,
        }
    }

    /// The record's request at process time, queued at C devAsyn's priority
    /// for its interface: devAsynOctet.c:764 queues at Medium; devAsynInt32
    /// (:838), devAsynInt64 (:657), devAsynFloat64 (:583), devAsynUInt32Digital
    /// (:678) and devAsynXXXArray.cpp:295 at Low.
    fn process_user(&self) -> AsynUser {
        let priority = match self.iface {
            Some(InterfaceType::Octet) => QueuePriority::Medium,
            _ => QueuePriority::Low,
        };
        self.request_user(priority)
    }

    /// The record's request at `init`: C device support reads there through
    /// SyncIO, whose `queueLockPort` queues at Low (asynManager.c:1896).
    fn init_user(&self) -> AsynUser {
        self.request_user(QueuePriority::Low)
            .locking_port(&self.handle)
    }

    /// C `pasyn<Xx>SyncIO->connect(port, addr, &user, userParam)`: given a
    /// userParam it runs the port's `drvUserCreate` again on its own asynUser
    /// (asynInt32SyncIO.c:94-110 and siblings), so the driver's create, and
    /// its trace, happens once per connect. A failure is the dset's `goto bad`.
    fn sync_io_connect(
        &self,
        drv_user: bool,
        sync_io: &'static str,
    ) -> Result<(), DeviceInitOutcome> {
        if !drv_user {
            return Ok(());
        }
        let req = DrvUserRequest::new(&self.drv_info, self.addr).with_iface(self.iface);
        match self.handle.drv_user_create_blocking(&req) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.dset.report(
                    &self.record_name,
                    &LinkRefusal::SyncIo(sync_io, drv_user_message(&e)),
                );
                Err(self.dset.bad())
            }
        }
    }

    fn request_user(&self, priority: QueuePriority) -> AsynUser {
        AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout_opt(self.timeout)
            .with_priority(priority)
            .with_drv_user(self.drv_user.clone())
    }

    /// Create with a typed interface from a [`PortHandle`].
    pub fn with_interface_handle(handle: PortHandle, link: AsynLink, iface: InterfaceType) -> Self {
        Self::from_handle(handle, link, iface.asyn_name())
    }

    /// The record's request on this port, made once — at `init`, so the
    /// first process allocates no more than the next.
    fn request_cell(&mut self) -> Arc<RequestCell> {
        match &self.request_cell {
            Some(cell) if self.handle.owns_cell(cell) => Arc::clone(cell),
            _ => {
                let _ = self.device_job.trace.set(ProcessTrace {
                    trace: self.handle.user_trace(),
                    record: self.record_name.clone(),
                    file: self.dset,
                    int32_mask: self.int32_mask,
                });
                let job: Arc<dyn CellBody> = self.device_job.clone();
                let cell = self.handle.new_cell(job);
                self.request_cell = Some(Arc::clone(&cell));
                cell
            }
        }
    }

    /// C `queueRequest` of the record's request (devAsynFloat64.c:583, :661):
    /// its gate, then the link. A request still queued from an earlier process
    /// is refused, as C refuses an `asynUser` that `isQueued`
    /// (asynManager.c:1586-1591).
    #[inline(always)]
    fn queue(&mut self, op: RequestOp, user: AsynUser, finish: Finish) -> AsynResult<()> {
        let cell = self.request_cell();
        // A refused op keeps its buffer for the next request.
        if let Err(e) = self.handle.queue_gate(&user) {
            self.device_job.keep_spare(op);
            return Err(self.report_queue_refusal(e.into()));
        }
        // The port takes this slot before it records or lands a result, so
        // holding it across the queue keeps the new one from being cleared.
        let mut slot = self.device_job.request.lock();
        if let Err(e) = cell.queue(&user) {
            drop(slot);
            self.device_job.keep_spare(op);
            return Err(self.report_queue_refusal(e.into()));
        }
        match finish {
            Finish::Complete => *self.device_job.wrote.lock() = false,
            Finish::Reprocess => *self.device_job.read.lock() = None,
        }
        *slot = Some((op, user, finish));
        Ok(())
    }

    /// C `reportQueueRequestStatus` for a refused `queueRequest`.
    #[cold]
    #[inline(never)]
    fn report_queue_refusal(&mut self, e: AsynError) -> AsynError {
        self.report_queue_status(e.status(), &e.message_display());
        e
    }

    /// C `reportQueueRequestStatus` (devAsynInt32.c:792-809,
    /// devAsynXXXArray.cpp:266-279): one `ASYN_TRACE_ERROR` line per change of
    /// `queueRequest` status. A blocking port's queued request returns before
    /// C reports, so only a synchronous port reports a success.
    fn report_queue_status(&mut self, status: AsynStatus, message: &dyn std::fmt::Display) {
        if status == self.queue_status {
            return;
        }
        self.queue_status = status;
        let (rec, driver) = (&self.record_name, self.dset.driver());
        let mask = crate::trace::TraceMask::ERROR;
        let array = self.dset == Dset::Array;
        let at = if array {
            ""
        } else {
            "::reportQueueRequestStatus"
        };
        if status == AsynStatus::Success {
            self.dev_print(
                mask,
                file!(),
                line!(),
                format_args!("{rec} {driver}{at} queueRequest status returned to normal\n"),
            );
        } else {
            let error = if array { "" } else { "error " };
            self.dev_print(
                mask,
                file!(),
                line!(),
                format_args!("{rec} {driver}{at} queueRequest {error}{message}\n"),
            );
        }
    }

    /// The record's input interrupt client: every callback lands in the ring
    /// and rings for its process, C's `scanIoRequest`.
    fn subscribe_input(&mut self) -> AsynResult<()> {
        // C parity: a UInt32Digital record registers its @asynMask as the
        // interrupt mask (devAsynUInt32Digital.c:293/343); the driver fires
        // the callback only when `pInterrupt->mask & interruptMask`
        // (asynPortDriver.cpp:720) and delivers `pInterrupt->mask & value`
        // (asynPortDriver.cpp:729). Other interfaces carry no changed-bit
        // mask (uint32_changed_mask == 0), so a mask filter would gate every
        // interrupt out — restrict the gate (and the value masking below) to
        // UInt32Digital.
        let is_uint32 = self.iface_type == "asynUInt32Digital";
        let mask = self.mask;
        // asynInt32 @asynMask nbits config, applied to interrupt values
        // the same way the polled read masks them (C interruptCallbackInput,
        // devAsynInt32.c:537-540). `None` for every non-int32 / no-nbits
        // case, so the closure leaves the value untouched.
        let int32_mask = self.int32_mask;
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.handle.param_list(self.addr)),
            user_addr: Some(self.addr),
            drv_user: self.drv_user.clone(),
            uint32_mask: if is_uint32 { Some(mask) } else { None },
            // Per-interface routing (C's per-interface interrupt lists): a
            // driver firing interface-typed values reaches this record only with
            // its own interface, so an asynUInt32Digital bi gets the raw word to
            // mask and an asynInt32 ai gets the int to ESLO-convert — not the one
            // collapsed value. A single-value driver fires untyped and still
            // reaches here (InterruptFilter::iface).
            iface: self.iface,
        };

        // Every callback lands in the ring, inline in the driver's notify()
        // like C's interruptCallbackInput, and the ring rings the
        // scanIoRequest doorbell that setup_io_intr() consumes.
        let fifo = self.interrupt_fifo.clone();
        let gate = self.gate.clone();
        let trace = self.handle.user_trace();
        let (addr, reason) = (self.addr, self.reason as i32);
        let (record, driver) = (self.record_name.clone(), self.dset.driver());
        let scalar = self.dset.is_scalar();
        let octet = self.dset == Dset::Octet;
        let ring = self.interrupt_fifo.lock().unwrap().size();
        let callback = if self.output {
            "interruptCallbackOutput"
        } else {
            "interruptCallbackInput"
        };
        let sub = self
            .handle
            .interrupts()
            .register_sync_callback(filter, move |iv| {
                // C parity (asynPortDriver.cpp:729 + devAsynUInt32Digital.c:463):
                // deliver `mask & value` for UInt32Digital so the I/O-Intr value
                // matches the polled read (RequestOp::UInt32DigitalRead { mask }).
                // interruptCallbackInput stores the already-masked value in the
                // ring buffer; mirror that here. For asynInt32 apply the
                // @asynMask nbits mask + sign-extend (devAsynInt32.c:537-540),
                // matching the polled `result_to_value` path. Other values
                // pass through.
                let value = match &iv.value {
                    crate::param::ParamValue::UInt32Digital(v) if is_uint32 => {
                        crate::param::ParamValue::UInt32Digital(v & mask)
                    }
                    crate::param::ParamValue::Int32(v) => {
                        crate::param::ParamValue::Int32(int32_mask.map_or(*v, |m| m.apply(*v)))
                    }
                    other => other.clone(),
                };
                // Printed before the interruptAccept test, as C does
                // (devAsynInt32.c:541-543, :592-594; devAsynOctet.c:485-488,
                // devAsynXXXArray.cpp:402-405).
                let io = crate::trace::TraceMask::IO_DEVICE;
                if scalar {
                    trace.print(
                        addr,
                        reason,
                        io,
                        file!(),
                        line!(),
                        format_args!(
                            "{record} {driver}::{callback} new value={}\n",
                            Scalar::delivered(&value)
                        ),
                    );
                } else if trace.manager.is_enabled_device(&trace.port, addr, io) {
                    let (len, data) = callback_data(&value, iv.octet_counts_nul());
                    trace.manager.output_device_io(
                        &trace.port,
                        Some(addr),
                        reason,
                        io,
                        &data,
                        &format!(
                            "{record} {driver}::interruptCallback ringSize={ring}, len={len}, callback data:"
                        ),
                        file!(),
                        line!(),
                    );
                }
                let open = gate.open();
                let entry = CachedInterrupt {
                    value,
                    timestamp: iv.timestamp,
                    // Carry the driver alarm so the IoIntr read path can
                    // recGblSetSevr it (C devAsynInt32.c:561-563/843-847);
                    // dropping it left I/O-Intr records permanently
                    // NO_ALARM even when the driver flagged an alarm.
                    alarm_status: iv.alarm_status,
                    alarm_severity: iv.alarm_severity,
                    // Carry the transport status too (C ring `rp->status =
                    // pasynUser->auxStatus`, devAsynInt32.c:600); the read maps
                    // it and gates the value store.
                    aux_status: iv.aux_status,
                    octet_nul: iv.octet_counts_nul(),
                };
                // C parity (devAsynInt32.c:564-576): an overflow drops the
                // oldest entry, leaving what is owed unchanged; a fresh entry
                // is owed one more process.
                let mut g = fifo.lock().unwrap();
                if open {
                    g.push_with_overflow(entry);
                    g.ring();
                } else if octet {
                    g.hold(entry);
                }
            });
        self.interrupt_sub = Some(sub?);
        Ok(())
    }

    /// C `initAiAverage`'s registration (devAsynInt32.c:870-872,
    /// devAsynFloat64.c:698-700): one SYNCHRONOUS callback in every SCAN, since
    /// averaging must see every sample. Each sample asks `isIOIntrScan` which
    /// of C's two models it feeds, so a SCAN move switches it.
    fn subscribe_average(&mut self) -> AsynResult<()> {
        let Some(acc) = self.average.clone() else {
            return Ok(());
        };
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.handle.param_list(self.addr)),
            user_addr: Some(self.addr),
            drv_user: self.drv_user.clone(),
            uint32_mask: None,
            // Averaging accumulates every sample; restrict to this record's
            // interface so a per-interface driver's parallel typed fires for
            // one poll are not each pushed as a separate sample.
            iface: self.iface,
        };
        // asynInt32Average applies the same @asynMask nbits the polled
        // read does (devAsynInt32.c:537-540); asynFloat64Average has no mask.
        let int32_mask = self.int32_mask;
        let is_int32 = self.iface_type == "asynInt32";

        // Mode 1 — SCAN="I/O Intr": C `interruptCallbackAverage` isIOIntrScan
        // branch (devAsynInt32.c:673-702). Accumulate every sample, and every
        // round(SVAL) samples decimate the mean into the IoIntr ring and
        // `scanIoRequest` the record; read() drains the ring
        // (`getCallbackValue`). The ring entry carries the TRIGGERING sample's
        // status/alarm — C `rp->status = pasynUser->auxStatus` (:685-687), NOT
        // the Mode 2 OR-accumulation.
        //
        // Mode 2 — periodic SCAN: the periodic record process drains the
        // running mean in read(). The callback OR-accumulates the transport
        // status and stashes the last sample's EPICS alarm
        // (devAsynInt32.c:705-707, devAsynFloat64.c:516-518) — once any sample
        // is non-success the period stays a transport error (asynSuccess == 0,
        // so the OR is sticky).
        let fifo = self.interrupt_fifo.clone();
        let gate = self.gate.clone();
        let io_intr_scan = self.io_intr_scan.clone();
        let trace = self.handle.user_trace();
        let (addr, reason) = (self.addr, self.reason as i32);
        let (record, driver) = (self.record_name.clone(), self.dset.driver());
        let sub = self
            .handle
            .interrupts()
            .register_sync_callback(filter, move |iv| {
                use std::sync::atomic::Ordering;
                let (shown, s) = match iv.value {
                    crate::param::ParamValue::Int32(v) => {
                        let v = int32_mask.map_or(v, |m| m.apply(v));
                        (Scalar::Int(v as i64), v as f64)
                    }
                    crate::param::ParamValue::Float64(v) => (Scalar::Float(v), v),
                    _ => return,
                };
                // Before the interruptAccept test (devAsynInt32.c:660-663,
                // devAsynFloat64.c:474-477).
                trace.print(
                    addr,
                    reason,
                    crate::trace::TraceMask::IO_DEVICE,
                    file!(),
                    line!(),
                    format_args!("{record} {driver}::interruptCallbackAverage new value={shown}\n"),
                );
                if !gate.open() {
                    return;
                }
                // C: numAverage++; sum += value (devAsynInt32.c:665-666).
                acc.averager.push(s);
                if !io_intr_scan.load(Ordering::Relaxed) {
                    let mut st = acc.status.lock().unwrap();
                    st.last_alarm = (iv.alarm_status, iv.alarm_severity);
                    if iv.aux_status != crate::error::AsynStatus::Success {
                        st.aux_error = Some(iv.aux_status);
                    }
                    return;
                }
                // C: numToAverage = (int)(sval+0.5), min 1; decimate
                // when numAverage >= numToAverage (devAsynInt32.c:674-676).
                let n = acc.num_to_average.load(Ordering::Relaxed).max(1) as u64;
                if acc.averager.count() < n {
                    return;
                }
                // C: dval = round(sum/numAverage); reset sum/count
                // (devAsynInt32.c:679-683). The checked drains reset
                // atomically and return Some (count >= n >= 1 here).
                let value = if is_int32 {
                    crate::param::ParamValue::Int32(
                        acc.averager.read_and_reset_int32_checked().unwrap_or(0),
                    )
                } else {
                    crate::param::ParamValue::Float64(
                        acc.averager.read_and_reset_checked().unwrap_or(0.0),
                    )
                };
                let entry = CachedInterrupt {
                    value,
                    timestamp: iv.timestamp,
                    // C takes the triggering sample's status/alarm, NOT
                    // an OR-accumulation (rp->status = pasynUser->
                    // auxStatus, devAsynInt32.c:685-687).
                    alarm_status: iv.alarm_status,
                    alarm_severity: iv.alarm_severity,
                    aux_status: iv.aux_status,
                    octet_nul: false,
                };
                // C: ring full → evict oldest + overflow, NO
                // scanIoRequest; fresh add → scanIoRequest
                // (devAsynInt32.c:688-701).
                let mut g = fifo.lock().unwrap();
                g.push_with_overflow(entry);
                g.ring();
            });
        self.average_callback_sub = Some(sub?);
        Ok(())
    }

    /// The record's asynEnum interrupt client (C `callbackEnum`'s
    /// `registerInterruptUser`, devAsynInt32.c:317-323).
    fn subscribe_enum(&mut self) -> AsynResult<InterruptReceiver> {
        // Subscribe to the enum param's interrupts. On a `ParamType::Enum`
        // param the enum value (index) and the enum table (choices) live on one
        // `ParamValue::Enum`, so both a value change and a `set_enum_choices`
        // change arrive here; C separates them across the asynInt32 and
        // asynEnum interfaces. The bridge below recovers that separation by
        // posting only when the *choices* differ from the last-applied table
        // (seeded with the init table) — so an int32 value change fires no
        // DBE_PROPERTY, matching C.
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.handle.param_list(self.addr)),
            user_addr: Some(self.addr),
            drv_user: self.drv_user.clone(),
            uint32_mask: None,
            // This IS the record's asynEnum interrupt client (C
            // `registerInterruptUser` on the asynEnum interface,
            // devAsynInt32.c:318): a table fired for a param of any other type
            // comes tagged asynEnum (`PortDriverBase::do_callbacks_enum`),
            // which the record's own value interface would gate out. The
            // untyped `call_param_callbacks` fire of an Enum param matches any
            // filter.
            iface: Some(crate::interfaces::InterfaceType::Enum),
        };
        // C's enum callbacks return before `setEnums` and the DBE_PROPERTY
        // post while `interruptAccept` is false (devAsynInt32.c:718/:732/
        // :746/:760, devAsynUInt32Digital.c:553/:567/:581/:595): the table
        // is dropped, and the next one after the gate opens is applied. The
        // gate is read when the callback fires, in the mailbox write, not
        // when the task below drains it.
        let gate = self.gate.clone();
        let (sub, rx) = self.handle.interrupts().register_interrupt_user_when(
            filter,
            MAX_ENUM_STATES * ENUM_ENTRY_BYTES,
            move || gate.open(),
        )?;
        self.enum_interrupt_sub = Some(sub);
        Ok(rx)
    }

    /// C devAsyn's line for a `registerInterruptUser` the port refused
    /// (`getInterruptPvt`, asynManager.c:2492-2497). C `printf`s it, except
    /// devAsynXXXArray.cpp:245 and devAsynXXXTimeSeries.h:162, which print it
    /// at `ASYN_TRACE_ERROR`.
    #[cold]
    fn report_register_refusal(&self, at: Register, e: &AsynError) {
        let msg = e.message_display();
        match self.dset.register_refusal_line(at, &self.record_name, &msg) {
            (line, true) => self.dev_print(
                crate::trace::TraceMask::ERROR,
                file!(),
                line!(),
                format_args!("{line}\n"),
            ),
            (line, false) => epics_libcom_rs::epics_println!("{line}"),
        }
    }

    /// The record's completion and the band its `PRIO` selects this pass, set
    /// before the request is queued.
    #[inline(always)]
    fn arm_reentry(&self, done: &DeviceDoneSource<'_>) {
        let mut reentry = self.device_job.reentry.lock();
        match &mut *reentry {
            Some((_, prio)) => *prio = done.priority(),
            None => *reentry = Some((done.device_done(), done.priority())),
        }
    }

    /// C `pPvt->isIOIntrScan`.
    fn is_io_intr_scan(&self) -> bool {
        self.io_intr_scan.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// C `getCallbackValue`'s test: the ring holds an entry. C does not ask
    /// whether the record is still on I/O Intr (devAsynInt32.c:768-790), so an
    /// entry left when SCAN moved off it is the next process's value.
    fn callback_value_waiting(&self) -> bool {
        self.interrupt_fifo.lock().unwrap().unread()
    }

    /// The result a queued read left for the re-entered pass, or one the queue
    /// refused in `read_begin` (C `reportQueueRequestStatus`, applied by the
    /// same `processAi` pass, devAsynInt32.c:842-847).
    fn take_ran(&self) -> Option<AsynResult<RequestResult>> {
        self.device_job.read.lock().take()
    }

    /// The write an `asynOctetWrite`/`asynOctetWriteBinary` waveform sends from
    /// its `process` (C `callbackWfWrite`/`callbackWfWriteBinary`).
    fn write_only_op(&self, record: &dyn Record) -> Option<RequestOp> {
        let val = self.device_output_value(record)?;
        if self.octet_binary {
            self.binary_write_op(record, &val)
        } else {
            self.write_op(&val)
        }
    }

    /// `T`'s buffer from the op the port finished with last, emptied: a
    /// record's requests have one shape, so the next op refills the last one's
    /// capacity, as C reuses the record's own buffer.
    fn recycled<T: OpData>(&self) -> Vec<T> {
        let spare = self.device_job.spare.lock().take();
        let mut buf = spare.and_then(T::buffer).unwrap_or_default();
        buf.clear();
        buf
    }

    /// Set the bit mask for UInt32Digital read/write operations.
    ///
    /// For the `asynInt32` interface the same `@asynMask` 3rd arg is a
    /// signed bit COUNT (nbits), not a raw bitmask (C devAsynInt32.c:
    /// 233-248): reinterpret the stored 32-bit pattern as `i32` and derive
    /// the mask + sign-extend + device-bounds config from it.
    pub fn with_mask(mut self, mask: u32) -> Self {
        self.mask = mask;
        if self.iface_type == "asynInt32" {
            self.int32_mask = Int32Mask::from_nbits(mask as i32);
        }
        self
    }

    /// Enable initial readback: on init, read the current value from the driver
    /// and set it on the record (for output records).
    pub fn with_initial_readback(mut self) -> Self {
        self.initial_readback = true;
        self
    }

    /// Enable `asyn:READBACK` mode (asyn upstream PRs #60 / #208).
    ///
    /// Manual override. The framework auto-parses
    /// `info("asyn:READBACK", "...")` from the record's info map via
    /// [`Self::apply_record_info`] — `wire_device_support` and
    /// `IocBuilder` both call that hook automatically, so this manual
    /// setter is only needed for callers that construct an adapter
    /// outside the IocApplication wiring (e.g. unit tests, embedded
    /// scripts). C asyn parity: matches `asynDbGetInfo(pr,
    /// "asyn:READBACK")` calls in `devAsynInt32.c:329`,
    /// `devAsynFloat64.c:218`, `devAsynInt64.c:257`,
    /// `devAsynUInt32Digital.c:286`, `devAsynOctet.c:337`,
    /// `devAsynXXXArray.cpp:172`.
    pub fn set_asyn_readback(&mut self, on: bool) {
        self.asyn_readback = on;
    }

    /// Override initial-readback mode. The framework auto-parses
    /// `info("asyn:INITIAL_READBACK", "...")` via
    /// [`Self::apply_record_info`] (mirror of `asynDbGetInfo(precord,
    /// "asyn:INITIAL_READBACK")` at `devAsynOctet.c:357`).
    pub fn set_initial_readback(&mut self, on: bool) {
        self.initial_readback = on;
    }

    /// Override the I/O Intr ring buffer size, as `info("asyn:FIFO", "<n>")`
    /// does through [`Self::apply_record_info`]; 0 is C's latest-value slot.
    /// Callbacks already stored are dropped.
    pub fn set_fifo_size(&mut self, n: usize) {
        self.fifo_tag = Some(n);
        self.settle_ring();
    }

    /// C `createRingBuffer`'s `ringSize`: `asyn:FIFO`, else the dset's
    /// `DEFAULT_RING_BUFFER_SIZE` — 10 in the scalar files
    /// (devAsynInt32.c:63), 0 in devAsynOctet (:73) and devAsynXXXArray
    /// (:43), and 1 on an `asyn:READBACK` octet output (devAsynOctet.c:342).
    ///
    /// Two C defects are not copied. A scalar ring of 0 overflows on every
    /// callback and never scans (devAsynInt32.c:565-571); here it is the
    /// latest-value slot. devAsynXXXArray allocates `ringSize` elements
    /// where the others allocate `ringSize+1` (devAsynXXXArray.cpp:214), so
    /// it holds one value fewer than asked and none at 1; here it holds N.
    fn settle_ring(&self) {
        let size = self.fifo_tag.unwrap_or(match self.dset {
            Dset::Octet if self.output && self.asyn_readback => 1,
            Dset::Octet | Dset::Array => 0,
            _ => DEFAULT_RING_BUFFER_SIZE,
        });
        self.interrupt_fifo.lock().unwrap().resize(size);
    }

    /// Set the driver info string (used for `drv_user_create` during init).
    /// Allows record-name-based device support to configure the adapter
    /// in `set_record_info()` before `init()` runs.
    pub fn set_drv_info(&mut self, drv_info: &str) {
        self.drv_info = drv_info.to_string();
    }

    /// Set the interface type string (e.g. "asynInt32", "asynFloat64").
    /// Allows record-name-based device support to configure the adapter
    /// in `set_record_info()` before `init()` runs.
    pub fn set_iface_type(&mut self, iface_type: &str) {
        self.iface_type = iface_type.to_string();
        self.iface = InterfaceType::from_asyn_name(iface_type);
    }

    /// Set the param reason (index) directly, skipping `drv_user_create` during init.
    /// Use when the caller already knows the param index.
    pub fn set_reason(&mut self, reason: usize) {
        self.reason = reason;
        self.reason_set = true;
    }

    /// Get the param reason (index).
    pub fn reason(&self) -> usize {
        self.reason
    }

    /// Get the asyn address.
    pub fn addr(&self) -> i32 {
        self.addr
    }

    /// Get a reference to the underlying port handle.
    pub fn handle(&self) -> &PortHandle {
        &self.handle
    }

    /// Build a write request op from an EpicsValue (public wrapper for subclasses).
    pub fn write_op_pub(&self, val: &EpicsValue) -> Option<RequestOp> {
        self.write_op(val)
    }
}

/// Mirror of C asyn `computeShift(epicsUInt32 mask)` at
/// `devAsynUInt32Digital.c:627-636`: returns the position of the
/// lowest set bit (0..=32). A mask of 0 falls through the loop and
/// returns 32 — but mbbi/mbbo treat 0 as "use the full word", so
/// callers should guard before invoking.
fn compute_mask_shift(mask: u32) -> u32 {
    let mut bit: u32 = 1;
    for i in 0..32 {
        if (mask & bit) != 0 {
            return i;
        }
        bit <<= 1;
    }
    32
}

/// Parse an `info(...)` tag value as a boolean.
///
/// Truthy: non-empty and not `0` / `no` / `false` (case-insensitive).
/// Mirrors the broader EPICS convention; C asyn uses `atoi` directly
/// (`devAsynInt32.c:330` — `enableCallbacks = atoi(callbackString)`),
/// which only treats `"0"` / non-numeric as falsey. Our parse is a
/// strict superset for the documented `"1"` / `"0"` values and
/// additionally accepts the human-friendly `"Y"` / `"true"` forms.
fn parse_info_bool(raw: &str) -> bool {
    let v = raw.trim();
    !v.is_empty()
        && !v.eq_ignore_ascii_case("0")
        && !v.eq_ignore_ascii_case("no")
        && !v.eq_ignore_ascii_case("false")
}

fn asyn_to_ca_error(e: AsynError) -> CaError {
    CaError::Protocol(e.to_string())
}

/// Convert an asyn error to an EPICS (alarm status, alarm severity) pair
/// for the device-support READ path.
///
/// Mirrors C `asynStatusToEpicsAlarm` (asynEpicsUtils.c:234-266) with the
/// READ-path defaults the Int32/Float64/UInt32 device support pass:
/// `defaultStat = READ_ALARM`, `defaultSevr = INVALID_ALARM`
/// (devAsynInt32.c:844-846). Every non-success asyn status maps to
/// INVALID severity; only the condition code differs:
/// - Success      → (NO_ALARM, NO_ALARM)
/// - Timeout      → (TIMEOUT_ALARM, INVALID)   = (10, 3)
/// - Overflow     → (HW_LIMIT_ALARM, INVALID)  = (11, 3)
/// - Disconnected → (COMM_ALARM, INVALID)      = (9, 3)
/// - Disabled     → (DISABLE_ALARM, INVALID)   = (18, 3)
/// - Error/other  → (READ_ALARM, INVALID)      = (1, 3)
///
/// Pre-fix this mapping was wrong: Timeout / Overflow / Error all yielded
/// (7, 2) — condition 7 is STATE_ALARM, not READ_ALARM (which is 1), and
/// MAJOR(2) instead of INVALID(3) — and Disabled was lumped with
/// Disconnected as COMM_ALARM(9) instead of DISABLE_ALARM(18).
/// Map an asyn transport status to the EPICS (alarm condition, severity) it
/// implies — C `asynStatusToEpicsAlarm` with `defaultSevr = INVALID_ALARM`
/// (asynEpicsUtils.c:238-265). `default_stat` is the condition C uses for the
/// `asynError`/unknown arm: `READ_ALARM` for input device support,
/// `WRITE_ALARM` for output. `Timeout`/`Overflow`/`Disconnected`/`Disabled`
/// map to fixed conditions regardless of direction; `asynSuccess` → (NO_ALARM,
/// NO severity). This is the per-status mapping only; C's fill-in rule
/// (overwrite a field only while it is still NO_ALARM, so a sample's own EPICS
/// alarm is preserved) is applied by callers that carry a base alarm.
fn asyn_status_to_alarm_with_default(
    status: crate::error::AsynStatus,
    default_stat: u16,
) -> (u16, u16) {
    use crate::error::AsynStatus;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::AlarmSeverity;
    let invalid = AlarmSeverity::Invalid as u16;
    match status {
        AsynStatus::Success => (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm as u16),
        AsynStatus::Timeout => (alarm_status::TIMEOUT_ALARM, invalid),
        AsynStatus::Overflow => (alarm_status::HW_LIMIT_ALARM, invalid),
        AsynStatus::Disconnected => (alarm_status::COMM_ALARM, invalid),
        AsynStatus::Disabled => (alarm_status::DISABLE_ALARM, invalid),
        AsynStatus::Error => (default_stat, invalid),
    }
}

/// Resolve the EPICS alarm an interrupt sample raises from its own EPICS alarm
/// (`sample_alarm`) and its asyn transport status (`aux`), mirroring C
/// `asynStatusToEpicsAlarm`'s fill-in (asynEpicsUtils.c:238-265): on
/// `asynSuccess` the sample's alarm passes through unchanged; otherwise the
/// transport-mapped alarm fills a field only while it is still `NO_ALARM`, so a
/// sample's own EPICS alarm wins. Shared by the averaging drain and the regular
/// I/O Intr ring read; each caller owns its own value-discard gate.
fn resolve_intr_alarm(
    sample_alarm: (u16, u16),
    aux: crate::error::AsynStatus,
    default_stat: u16,
) -> (u16, u16) {
    if aux == crate::error::AsynStatus::Success {
        return sample_alarm;
    }
    let (mstat, msev) = asyn_status_to_alarm_with_default(aux, default_stat);
    (
        if sample_alarm.0 == 0 {
            mstat
        } else {
            sample_alarm.0
        },
        if sample_alarm.1 == 0 {
            msev
        } else {
            sample_alarm.1
        },
    )
}

/// Map an asyn transport error to an EPICS alarm with an explicit direction
/// default. C `devAsynOctet` (processCommon, devAsynOctet.c:806-807) maps a
/// transfer failure with `pPvt->isOutput ? WRITE_ALARM : READ_ALARM`, so an
/// output (write-only) record raises WRITE_ALARM where an input read raises
/// READ_ALARM. `default_stat` carries that direction; it feeds both the
/// `asynError`/unknown arm and the status-mapped `asynError`/default within
/// [`asyn_status_to_alarm_with_default`] (specific statuses —
/// Timeout/Overflow/Disconnected/Disabled — map direction-independently).
fn asyn_error_to_alarm_with_default(e: &AsynError, default_stat: u16) -> (u16, u16) {
    // `AsynError::status()` is the single owner of error → asynStatus, so a
    // status-carrying variant (e.g. `PartialWrite`, which reports a real
    // timeout alongside the bytes the device accepted) keeps its condition
    // code instead of silently falling into the generic asynError branch.
    // Non-status variants map to `AsynStatus::Error`, which
    // `asyn_status_to_alarm_with_default` resolves to C's default branch.
    asyn_status_to_alarm_with_default(e.status(), default_stat)
}

/// [`asyn_error_to_alarm_with_default`] with C's input default (`READ_ALARM`).
fn asyn_error_to_alarm(e: &AsynError) -> (u16, u16) {
    asyn_error_to_alarm_with_default(e, epics_base_rs::server::recgbl::alarm_status::READ_ALARM)
}

/// Convert a native-typed array interrupt to the element type of the consuming
/// record's asyn array interface.
///
/// C `NDPluginStdArrays::processCallbacks` fires an interrupt on every one of
/// the six array interfaces, each running `pNDArrayPool->convert(pArray, ...,
/// signedType)` so a record on any interface receives its own type-converted
/// copy (NDPluginStdArrays.cpp:170-197). The Rust runtime carries a single
/// native-typed array per param; this routes it through the same per-interface
/// `convert` the polled `readArray` path uses ([`AsynDeviceSupport::result_to_value`]),
/// so an I/O Intr waveform whose FTVL differs from the array's native element
/// type still gets correctly-converted data each frame instead of the raw
/// native array. Returns `None` for non-array interfaces (scalar records keep
/// `param_value_for_iface`).
fn convert_param_array_to_iface(
    iface_type: &str,
    pv: &crate::param::ParamValue,
) -> Option<EpicsValue> {
    use crate::param::ParamValue;
    // `src as $t` is exactly the polled converter: integer->integer is a
    // truncating C cast, float->integer saturates, and casts to float round —
    // matching `copy_ccast`/`copy_convert` in the plugin runtime.
    /// The parameter's buffer as `T` elements: shared as it is when the
    /// parameter already holds `T`, cast element by element otherwise.
    fn share_or_cast<S: Copy + 'static, T: Copy + 'static>(
        a: &Arc<[S]>,
        cast: impl Fn(S) -> T,
    ) -> epics_base_rs::types::SharedArray<T> {
        match (a as &dyn std::any::Any).downcast_ref::<Arc<[T]>>() {
            Some(same) => epics_base_rs::types::SharedArray::from(Arc::clone(same)),
            None => a.iter().map(|&x| cast(x)).collect(),
        }
    }
    macro_rules! cast_vec {
        ($t:ty) => {{
            let v: epics_base_rs::types::SharedArray<$t> = match pv {
                ParamValue::Int8Array(a) => share_or_cast(a, |x| x as $t),
                ParamValue::Int16Array(a) => share_or_cast(a, |x| x as $t),
                ParamValue::Int32Array(a) => share_or_cast(a, |x| x as $t),
                ParamValue::Int64Array(a) => share_or_cast(a, |x| x as $t),
                ParamValue::Float32Array(a) => share_or_cast(a, |x| x as $t),
                ParamValue::Float64Array(a) => share_or_cast(a, |x| x as $t),
                _ => return None,
            };
            v
        }};
    }
    match iface_type {
        // i8 then reinterpret to u8, matching result_to_value's CharArray mapping.
        "asynInt8Array" => Some(EpicsValue::CharArray(
            cast_vec!(i8).into_iter().map(|x| x as u8).collect(),
        )),
        "asynInt16Array" => Some(EpicsValue::ShortArray(cast_vec!(i16))),
        "asynInt32Array" => Some(EpicsValue::LongArray(cast_vec!(i32))),
        // i64 then down-cast to i32, matching result_to_value's asynInt64Array mapping.
        "asynInt64Array" => Some(EpicsValue::LongArray(
            cast_vec!(i64).into_iter().map(|x| x as i32).collect(),
        )),
        "asynFloat32Array" => Some(EpicsValue::FloatArray(cast_vec!(f32))),
        "asynFloat64Array" => Some(EpicsValue::DoubleArray(cast_vec!(f64))),
        _ => None,
    }
}

/// `a` as `T` elements, its whole capacity kept: shared when it already
/// holds `T`, cast element by element otherwise.
fn share_or_cast_buf<S: Copy + 'static, T: Copy + 'static>(
    a: epics_base_rs::types::SharedArray<S>,
    cast: impl Fn(S) -> T,
) -> epics_base_rs::types::SharedArray<T> {
    use epics_base_rs::types::SharedArray;
    if let Some(same) = (&a as &dyn std::any::Any).downcast_ref::<SharedArray<T>>() {
        return same.clone();
    }
    let mut out: SharedArray<T> = a.buffer().iter().map(|&x| cast(x)).collect();
    out.truncate(a.len());
    out
}

/// The record's array buffer as `$t` elements for an array read, as C passes
/// `bptr` (devAsynXXXArray.cpp:352); a same-width FTVL C reinterprets is an
/// `as` cast. A value that is no array gives an empty buffer.
macro_rules! record_read_buf {
    ($val:expr, $t:ty) => {
        match $val {
            Some(EpicsValue::CharArray(a)) | Some(EpicsValue::UCharArray(a)) => {
                share_or_cast_buf(a, |x| x as $t)
            }
            Some(EpicsValue::ShortArray(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::UShortArray(a)) | Some(EpicsValue::EnumArray(a)) => {
                share_or_cast_buf(a, |x| x as $t)
            }
            Some(EpicsValue::LongArray(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::ULongArray(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::Int64Array(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::UInt64Array(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::FloatArray(a)) => share_or_cast_buf(a, |x| x as $t),
            Some(EpicsValue::DoubleArray(a)) => share_or_cast_buf(a, |x| x as $t),
            _ => epics_base_rs::types::SharedArray::default(),
        }
    };
}

/// An element type a request op carries its data in.
trait OpData: Sized {
    fn buffer(op: RequestOp) -> Option<Vec<Self>>;
}

macro_rules! op_data {
    ($t:ty => $($variant:ident)|+) => {
        impl OpData for $t {
            fn buffer(op: RequestOp) -> Option<Vec<Self>> {
                match op {
                    $(RequestOp::$variant { data, .. })|+ => Some(data),
                    _ => None,
                }
            }
        }
    };
}
op_data!(u8 => OctetWrite | OctetWriteRead | OctetWriteBinary);
op_data!(i8 => Int8ArrayWrite);
op_data!(i16 => Int16ArrayWrite);
op_data!(i32 => Int32ArrayWrite);
op_data!(i64 => Int64ArrayWrite);
op_data!(f32 => Float32ArrayWrite);
op_data!(f64 => Float64ArrayWrite);

/// Whether `op` carries a buffer [`AsynDeviceSupport::recycled`] can refill; only those are kept
/// as the spare.
fn carries_buffer(op: &RequestOp) -> bool {
    matches!(
        op,
        RequestOp::OctetWrite { .. }
            | RequestOp::OctetWriteRead { .. }
            | RequestOp::OctetWriteBinary { .. }
            | RequestOp::Int8ArrayWrite { .. }
            | RequestOp::Int16ArrayWrite { .. }
            | RequestOp::Int32ArrayWrite { .. }
            | RequestOp::Int64ArrayWrite { .. }
            | RequestOp::Float32ArrayWrite { .. }
            | RequestOp::Float64ArrayWrite { .. }
    )
}

/// How the record's cycle ends once its queued request has run.
#[derive(Clone, Copy)]
enum Finish {
    /// A put's write: the framework runs the cycle's tail.
    Complete,
    /// An input's `process` re-enters with PACT set and consumes the result,
    /// as C's second `processAi` pass does (devAsynInt32.c:831-856).
    Reprocess,
}

/// C devAsyn's request: the record's one `asynUser`, queued from `process`
/// (devAsynInt32.c:838, devAsynFloat64.c:661). Queued as a [`RequestCell`],
/// whose queue slot is reserved when the cell is made, so a process on a
/// blocking port queues its request under the record's hold without
/// allocating.
#[derive(Default)]
struct DeviceJob {
    request: parking_lot::Mutex<Option<(RequestOp, AsynUser, Finish)>>,
    /// A put's write ran and `done` has yet to complete the cycle.
    wrote: parking_lot::Mutex<bool>,
    /// The last put's [`write_alarm`], which the completing pass raises.
    write_alarm: parking_lot::Mutex<Option<Result<(u16, u16), AsynStatus>>>,
    /// A read's result until the re-entered pass consumes it; apart from
    /// `wrote`, so a write never moves a `RequestResult`.
    read: parking_lot::Mutex<Option<AsynResult<RequestResult>>>,
    /// The last request's op once the port is done with it, so the next one
    /// refills its buffer instead of allocating one ([`AsynDeviceSupport::recycled`]).
    spare: parking_lot::Mutex<Option<RequestOp>>,
    /// The record's completion and its band, C `pPvt->processCallback` and
    /// `pr->prio`; set by the process that queued the request.
    reentry:
        parking_lot::Mutex<Option<(DeviceDone, epics_libcom_rs::runtime::task::CallbackPriority)>>,
    /// C `pPvt->lastStatus`, which a process and the port thread's
    /// `processCallbackOutput` both update (devAsynInt32.c:494-500, :519-525).
    last_status: LastStatus,
    /// Set with the record's request cell.
    trace: std::sync::OnceLock<ProcessTrace>,
    /// `asynOctetWriteRead`'s command record, read where the request runs.
    write_read: std::sync::OnceLock<WriteReadSource>,
}

/// C devAsynOctet's `asynOctetWriteRead` command: the record `userParam` names
/// (`initDbAddr`), read with `dbGet(DBR_STRING)` and escape-translated each
/// process (devAsynOctet.c:878-899).
struct WriteReadSource {
    pv: String,
    /// C `callbackWfWriteRead` prints its function name; the stringin and lsi
    /// callbacks do not.
    waveform: bool,
    db: std::sync::OnceLock<epics_base_rs::server::database::AsyncDbHandle>,
}

impl WriteReadSource {
    /// The command bytes up to the translated string's NUL, or `None` for a
    /// failed `dbGet`. `held` is the record the caller holds.
    fn command(&self, held: Option<(&[u8], &dyn Record)>) -> Option<Vec<u8>> {
        let raw = self.db.get()?.get_dbr_string(&self.pv, held)?;
        let mut cmd = crate::asyn_record::translate_escape(&raw.as_str_lossy());
        if let Some(nul) = cmd.iter().position(|&b| b == 0) {
            cmd.truncate(nul);
        }
        Some(cmd)
    }

    /// Put the command into `op`'s write buffer; `false` is a failed `dbGet`.
    fn fill(&self, op: &mut RequestOp, held: Option<(&[u8], &dyn Record)>) -> bool {
        let RequestOp::OctetWriteRead { data, .. } = op else {
            return true;
        };
        let Some(cmd) = self.command(held) else {
            return false;
        };
        data.clear();
        data.extend_from_slice(&cmd);
        true
    }

    /// The failed `dbGet`'s line and `recGblSetSevr(READ_ALARM, INVALID_ALARM)`,
    /// with neither write nor read made; the result is still a success.
    fn failed(&self, line: Option<&ProcessLine<'_>>) -> AsynResult<RequestResult> {
        if let Some(line) = line {
            self.print_failed(line);
        }
        Ok(RequestResult {
            alarm_status: epics_base_rs::server::recgbl::alarm_status::READ_ALARM,
            alarm_severity: epics_base_rs::server::record::AlarmSeverity::Invalid as u16,
            ..RequestResult::write_ok()
        })
    }

    fn print_failed(&self, line: &ProcessLine<'_>) {
        let rec = line.record;
        if self.waveform {
            line.print(
                crate::trace::TraceMask::ERROR,
                file!(),
                line!(),
                format_args!("{rec} devAsynOctet::callbackWfWriteRead dbGet failed\n"),
            );
        } else {
            line.print(
                crate::trace::TraceMask::ERROR,
                file!(),
                line!(),
                format_args!("{rec} dbGet failed\n"),
            );
        }
    }
}

/// C `pPvt->lastStatus`: a failure prints once per change of status, not once
/// per process.
struct LastStatus(parking_lot::Mutex<AsynStatus>);

impl Default for LastStatus {
    fn default() -> Self {
        Self(parking_lot::Mutex::new(AsynStatus::Success))
    }
}

/// What the record's request callback needs on the port thread: the names its
/// lines print and the asynInt32 `@asynMask` its read applies.
struct ProcessTrace {
    trace: crate::user::UserTrace,
    record: String,
    file: Dset,
    int32_mask: Option<Int32Mask>,
}

/// What a request handed the driver. `op` is absent when a synchronous
/// request's buffer was not kept because nothing traces it.
struct Sent<'a> {
    op: Option<&'a RequestOp>,
    write: bool,
    read: bool,
    len: usize,
}

impl<'a> Sent<'a> {
    fn of(op: &'a RequestOp) -> Self {
        Self {
            op: Some(op),
            ..Sent::bare(op)
        }
    }

    fn bare(op: &RequestOp) -> Sent<'static> {
        let (write, read, len) = match op {
            RequestOp::OctetWrite { data } | RequestOp::OctetWriteBinary { data } => {
                (true, false, data.len())
            }
            RequestOp::OctetWriteRead { data, .. } => (true, true, data.len()),
            RequestOp::Int32Write { .. }
            | RequestOp::Int64Write { .. }
            | RequestOp::Float64Write { .. }
            | RequestOp::UInt32DigitalWrite { .. }
            | RequestOp::Int8ArrayWrite { .. }
            | RequestOp::Int16ArrayWrite { .. }
            | RequestOp::Int32ArrayWrite { .. }
            | RequestOp::Int64ArrayWrite { .. }
            | RequestOp::Float32ArrayWrite { .. }
            | RequestOp::Float64ArrayWrite { .. } => (true, false, 0),
            _ => (false, true, 0),
        };
        Sent {
            op: None,
            write,
            read,
            len,
        }
    }

    /// C writeIt's `nbytes != nbytesTransferred` (devAsynOctet.c:680): the
    /// port returns a short write without read data, as the write-read's
    /// read is never made.
    fn came_up_short(&self, r: &RequestResult) -> bool {
        self.write && self.len != r.nbytes && r.octet().is_none()
    }

    /// The short write's `recGblSetSevr(precord, WRITE_ALARM, MINOR_ALARM)`
    /// (devAsynOctet.c:685), folded into the result whose own alarm
    /// processCommon raises after it (:806-809): equal severity keeps
    /// WRITE, a higher one replaces it.
    fn raise_short_write(&self, file: Dset, ran: &mut AsynResult<RequestResult>) {
        use epics_base_rs::server::{recgbl::alarm_status, record::AlarmSeverity};
        let minor = AlarmSeverity::Minor as u16;
        if let Ok(r) = ran {
            if file == Dset::Octet && self.came_up_short(r) && r.alarm_severity <= minor {
                r.alarm_status = alarm_status::WRITE_ALARM;
                r.alarm_severity = minor;
            }
        }
    }

    fn bytes(&self) -> Option<&'a [u8]> {
        match self.op? {
            RequestOp::OctetWrite { data }
            | RequestOp::OctetWriteBinary { data }
            | RequestOp::OctetWriteRead { data, .. } => Some(data),
            _ => None,
        }
    }
}

/// The element count and the bytes of an octet or array callback, as C's
/// `interruptCallback` hands them to `asynPrintIO`: a parameter callback's
/// octet counts its NUL (asynPortDriver.cpp:817), and an array's elements are
/// in memory order.
fn callback_data(value: &crate::param::ParamValue, octet_nul: bool) -> (usize, Vec<u8>) {
    use crate::param::ParamValue as V;
    fn elems<T: Copy, const N: usize>(a: &[T], bytes: fn(T) -> [u8; N]) -> (usize, Vec<u8>) {
        (a.len(), ne_bytes(a, bytes))
    }
    match value {
        V::Octet(b) => {
            let mut data = b.to_vec();
            if octet_nul {
                data.push(0);
            }
            (data.len(), data)
        }
        V::Int8Array(a) => elems(a, i8::to_ne_bytes),
        V::Int16Array(a) => elems(a, i16::to_ne_bytes),
        V::Int32Array(a) => elems(a, i32::to_ne_bytes),
        V::Int64Array(a) => elems(a, i64::to_ne_bytes),
        V::UInt64Array(a) => elems(a, u64::to_ne_bytes),
        V::Float32Array(a) => elems(a, f32::to_ne_bytes),
        V::Float64Array(a) => elems(a, f64::to_ne_bytes),
        _ => (0, Vec::new()),
    }
}

/// An array's elements as they sit in C memory.
fn ne_bytes<T: Copy, const N: usize>(a: &[T], bytes: fn(T) -> [u8; N]) -> Vec<u8> {
    a.iter().flat_map(|&x| bytes(x)).collect()
}

/// The first `n` elements of an array field, as C's `bptr` holds them.
fn record_array_bytes(v: &EpicsValue, n: usize) -> Vec<u8> {
    fn head<T: Copy, const N: usize>(a: &[T], n: usize, bytes: fn(T) -> [u8; N]) -> Vec<u8> {
        ne_bytes(&a[..n.min(a.len())], bytes)
    }
    match v {
        EpicsValue::CharArray(a) | EpicsValue::UCharArray(a) => head(a, n, u8::to_ne_bytes),
        EpicsValue::ShortArray(a) => head(a, n, i16::to_ne_bytes),
        EpicsValue::UShortArray(a) | EpicsValue::EnumArray(a) => head(a, n, u16::to_ne_bytes),
        EpicsValue::LongArray(a) => head(a, n, i32::to_ne_bytes),
        EpicsValue::ULongArray(a) => head(a, n, u32::to_ne_bytes),
        EpicsValue::Int64Array(a) => head(a, n, i64::to_ne_bytes),
        EpicsValue::UInt64Array(a) => head(a, n, u64::to_ne_bytes),
        EpicsValue::FloatArray(a) => head(a, n, f32::to_ne_bytes),
        EpicsValue::DoubleArray(a) => head(a, n, f64::to_ne_bytes),
        _ => Vec::new(),
    }
}

/// A scalar as C's `processCallback*` formats `pPvt->result.value`: `%d`,
/// `%lld`, `%u`, and `%f` for asynFloat64.
enum Scalar {
    Int(i64),
    UInt(u32),
    Float(f64),
    None,
}

impl Scalar {
    fn sent(op: Option<&RequestOp>) -> Self {
        match op {
            Some(RequestOp::Int32Write { value }) => Self::Int(*value as i64),
            Some(RequestOp::Int64Write { value }) => Self::Int(*value),
            Some(RequestOp::UInt32DigitalWrite { value, .. }) => Self::UInt(*value),
            Some(RequestOp::Float64Write { value }) => Self::Float(*value),
            _ => Self::None,
        }
    }

    /// A value an interrupt delivered, as `interruptCallback*` prints it.
    fn delivered(v: &crate::param::ParamValue) -> Self {
        use crate::param::ParamValue as V;
        match *v {
            V::Int32(v) => Self::Int(v as i64),
            V::Int64(v) => Self::Int(v),
            V::Enum { index, .. } => Self::Int(index as i64),
            V::UInt32Digital(v) => Self::UInt(v),
            V::Float64(v) => Self::Float(v),
            _ => Self::None,
        }
    }

    fn read(r: &RequestResult) -> Self {
        if let Some(v) = r.float_val {
            Self::Float(v)
        } else if let Some(v) = r.int64_val {
            Self::Int(v)
        } else if let Some(v) = r.int_val {
            Self::Int(v as i64)
        } else if let Some(v) = r.uint_val {
            Self::UInt(v)
        } else {
            Self::None
        }
    }
}

impl std::fmt::Display for Scalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Int(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{}", CFloat(v)),
            Self::None => Ok(()),
        }
    }
}

/// Where a finished request prints: the record's user on its port.
struct ProcessLine<'a> {
    trace: &'a crate::user::UserTrace,
    record: &'a str,
    file: Dset,
    addr: i32,
    reason: i32,
}

impl ProcessLine<'_> {
    fn print(
        &self,
        mask: crate::trace::TraceMask,
        file: &str,
        line: u32,
        args: std::fmt::Arguments<'_>,
    ) {
        self.trace
            .print(self.addr, self.reason, mask, file, line, args);
    }

    /// C `asynPrintIO(pasynUser, …)`.
    fn print_io(
        &self,
        mask: crate::trace::TraceMask,
        data: &[u8],
        file: &str,
        line: u32,
        label: std::fmt::Arguments<'_>,
    ) {
        let t = self.trace;
        if t.manager.is_enabled_device(&t.port, self.addr, mask) {
            t.manager.output_device_io(
                &t.port,
                Some(self.addr),
                self.reason,
                mask,
                data,
                &label.to_string(),
                file,
                line,
            );
        }
    }

    /// The lines C prints once the request ran, in the callback that ran it:
    /// `processCallbackOutput`/`processCallbackInput` (devAsynInt32.c:475-528,
    /// devAsynInt64.c:371-418, devAsynUInt32Digital.c:385-434,
    /// devAsynFloat64.c:305-351), `writeIt`/`readIt` (devAsynOctet.c:662-717)
    /// and `queueRequestCallback` (devAsynXXXArray.cpp:343-372). The
    /// scalar and array failures print once per change of
    /// `pPvt->lastStatus`; the octet ones every time.
    fn finished(
        &self,
        last: &LastStatus,
        sent: &Sent<'_>,
        ran: Result<&RequestResult, &AsynError>,
    ) {
        use crate::trace::TraceMask;
        let rec = self.record;
        // C prints `pasynUser->errorMessage`, the driver's own text.
        let failed = ran.map_or_else(Some, RequestResult::octet_error);
        let error_message = failed.map(AsynError::message_display);
        let failure: Option<(AsynStatus, &dyn std::fmt::Display)> = match (failed, &error_message) {
            (Some(e), Some(message)) => Some((e.status(), message)),
            _ => None,
        };
        let mut last = last.0.lock();
        match (self.file, failure, ran) {
            (Dset::Octet, Some((_, message)), _) => {
                use crate::error::OctetStep;
                let step = match failed.and_then(AsynError::octet_step) {
                    Some(step) => step,
                    None if sent.read => OctetStep::Read,
                    None => OctetStep::Write,
                };
                // A command-response's write went out before its read failed:
                // C's writeIt printed it first.
                if step == OctetStep::Read && sent.write {
                    if let Some(data) = sent.bytes() {
                        self.print_io(
                            TraceMask::IO_DEVICE,
                            data,
                            file!(),
                            line!(),
                            format_args!("{rec} devAsynOctet::writeIt\n"),
                        );
                    }
                }
                let function = match step {
                    OctetStep::Read => "readIt",
                    // devAsynOctet never flushes; a flush is the write's.
                    OctetStep::Flush | OctetStep::Write => "writeIt",
                };
                self.print(
                    TraceMask::ERROR,
                    file!(),
                    line!(),
                    format_args!("{rec} devAsynOctet::{function} failed {message}\n"),
                );
            }
            (Dset::Octet, None, Ok(r)) => {
                if sent.write {
                    if sent.came_up_short(r) {
                        self.print(
                            TraceMask::ERROR,
                            file!(),
                            line!(),
                            format_args!(
                                "{rec} devAsynOctet::writeIt requested {} but sent {} bytes\n",
                                sent.len, r.nbytes
                            ),
                        );
                    } else if let Some(data) = sent.bytes() {
                        self.print_io(
                            TraceMask::IO_DEVICE,
                            data,
                            file!(),
                            line!(),
                            format_args!("{rec} devAsynOctet::writeIt\n"),
                        );
                    }
                }
                if let (true, Some(read)) = (sent.read, r.octet()) {
                    self.print_io(
                        TraceMask::IO_DEVICE,
                        read.data(),
                        file!(),
                        line!(),
                        format_args!(
                            "{rec} devAsynOctet::readIt eomReason {}\n",
                            read.eom().bits()
                        ),
                    );
                }
            }
            (Dset::Array, None, _) => self.print(
                TraceMask::IO_DEVICE,
                file!(),
                line!(),
                format_args!("{rec} devAsynXXXArray::queueRequestCallback OK\n"),
            ),
            (Dset::Array, Some((status, message)), _) if status != *last => {
                let verb = if sent.write { "write" } else { "read" };
                self.print(
                    TraceMask::ERROR,
                    file!(),
                    line!(),
                    format_args!(
                        "{rec} devAsynXXXArray::queueRequestCallback {verb} error {message}\n"
                    ),
                );
            }
            (scalar, None, ran) if scalar.is_scalar() => {
                let driver = scalar.driver();
                if sent.write {
                    let value = Scalar::sent(sent.op);
                    self.print(
                        TraceMask::IO_DEVICE,
                        file!(),
                        line!(),
                        format_args!(
                            "{rec} {driver}::processCallbackOutput process value {value}\n"
                        ),
                    );
                } else if let Ok(r) = ran {
                    let value = Scalar::read(r);
                    self.print(
                        TraceMask::IO_DEVICE,
                        file!(),
                        line!(),
                        format_args!(
                            "{rec} {driver}::processCallbackInput process value={value}\n"
                        ),
                    );
                }
            }
            // devBusyAsyn keeps no `lastStatus` (devBusyAsyn.c:322-326).
            (scalar, Some((status, message)), _)
                if scalar.is_scalar()
                    && (status != *last || scalar == Dset::Int32 { busy: true }) =>
            {
                let driver = scalar.driver();
                let (function, what) = match (sent.write, scalar) {
                    (true, Dset::Int64 | Dset::Int32 { busy: true }) => {
                        ("processCallbackOutput", "process error")
                    }
                    (true, _) => ("processCallbackOutput", "process write error"),
                    (false, _) => ("processCallbackInput", "process read error"),
                };
                self.print(
                    TraceMask::ERROR,
                    file!(),
                    line!(),
                    format_args!("{rec} {driver}::{function} {what} {message}\n"),
                );
            }
            _ => {}
        }
        *last = failure.map_or(AsynStatus::Success, |(status, _)| status);
    }
}

impl DeviceJob {
    /// Keep `op` for the next request to refill, if it has a buffer to give.
    fn keep_spare(&self, op: RequestOp) {
        if carries_buffer(&op) {
            *self.spare.lock() = Some(op);
        }
    }

    fn ran(&self, finish: Finish, ran: AsynResult<RequestResult>) {
        match finish {
            Finish::Complete => {
                *self.write_alarm.lock() = Some(write_alarm(&ran));
                *self.wrote.lock() = true;
            }
            Finish::Reprocess => *self.read.lock() = Some(ran),
        }
    }
}

impl CellBody for DeviceJob {
    fn process(&self, port: &mut dyn PortCalls) {
        let (mut op, user, finish) = self
            .request
            .lock()
            .take()
            .expect("a request is queued with its op");
        let (addr, reason) = (user.addr, user.reason as i32);
        let line = self.trace.get().map(|t| ProcessLine {
            trace: &t.trace,
            record: &t.record,
            file: t.file,
            addr,
            reason,
        });
        // The port thread holds no record: the command record is read here.
        let ran = match self.write_read.get() {
            Some(src) if !src.fill(&mut op, None) => src.failed(line.as_ref()),
            _ => {
                let mut ran = port.call(&op, user);
                // C prints in the callback the port thread ran.
                if let (Some(t), Some(line)) = (self.trace.get(), &line) {
                    if let Ok(result) = &mut ran {
                        Int32Mask::apply_to_read(t.int32_mask, result);
                    }
                    let sent = Sent::of(&op);
                    sent.raise_short_write(t.file, &mut ran);
                    line.finished(&self.last_status, &sent, ran.as_ref());
                }
                ran
            }
        };
        self.keep_spare(op);
        self.ran(finish, ran);
    }

    fn unrun(&self, why: Unrun) {
        let Some((op, _, finish)) = self.request.lock().take() else {
            return;
        };
        self.keep_spare(op);
        // Nothing cancels the request and it has no timeout callback, so only
        // a port shutting down with it queued leaves it unrun.
        let e = match why {
            Unrun::Refused(e) => e,
            Unrun::TimedOut | Unrun::Canceled => unreachable!("devAsyn request {why:?}"),
        };
        self.ran(finish, Err(e));
    }

    /// devAsynInt32.c:212, devAsynInt64.c:192, devAsynUInt32Digital.c:193,
    /// devAsynFloat64.c:152, devAsynOctet.c:273: `createAsynUser(callback, 0)`.
    /// A request on a disconnected device runs, and the driver's refusal is
    /// what the dset reports.
    fn timeout_user(&self) -> bool {
        false
    }

    /// C `processCallbackOutput`/`processCallbackInput`'s last line,
    /// `callbackRequestProcessCallback(&pPvt->processCallback, pr->prio, pr)`,
    /// on the port thread: devAsynInt32.c:500, :526, devAsynInt64.c:418,
    /// devAsynUInt32Digital.c:434, devAsynFloat64.c:351, devAsynOctet.c:830.
    /// Under the request slot, so a process queued meanwhile has already
    /// cleared this request's result rather than receiving it.
    fn done(self: Arc<Self>) {
        let _slot = self.request.lock();
        let wrote = std::mem::take(&mut *self.wrote.lock());
        let reentry = self.reentry.lock();
        let Some((reentry, prio)) = &*reentry else {
            *self.read.lock() = None;
            return;
        };
        if wrote {
            let _ = reentry.complete(*prio);
        } else if self.read.lock().is_some() {
            // The result stays for the re-entered pass's `read()`.
            let _ = reentry.reprocess(*prio);
        }
    }
}

/// mbbi/mbbo state-string fields ZRST..FFST in state order (16 states).
/// C `setEnums` writes the driver's enum strings starting at `&pr->zrst`
/// (devAsynInt32.c:720) into this contiguous block.
const MBB_STRING_FIELDS: [&str; 16] = [
    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST", "EIST", "NIST", "TEST", "ELST",
    "TVST", "TTST", "FTST", "FFST",
];
/// mbbi/mbbo state raw-value fields ZRVL..FFVL (parallel to [`MBB_STRING_FIELDS`]).
const MBB_VALUE_FIELDS: [&str; 16] = [
    "ZRVL", "ONVL", "TWVL", "THVL", "FRVL", "FVVL", "SXVL", "SVVL", "EIVL", "NIVL", "TEVL", "ELVL",
    "TVVL", "TTVL", "FTVL", "FFVL",
];
/// mbbi/mbbo state severity fields ZRSV..FFSV (parallel to [`MBB_STRING_FIELDS`]).
const MBB_SEVERITY_FIELDS: [&str; 16] = [
    "ZRSV", "ONSV", "TWSV", "THSV", "FRSV", "FVSV", "SXSV", "SVSV", "EISV", "NISV", "TESV", "ELSV",
    "TVSV", "TTSV", "FTSV", "FFSV",
];
/// bi/bo state-string fields ZNAM/ONAM (2 states; C `initBi` passes
/// `maxEnums=2`, `&pr->znam`, devAsynInt32.c:1138-1140).
const BI_STRING_FIELDS: [&str; 2] = ["ZNAM", "ONAM"];
/// bi/bo state severity fields ZSV/OSV (bi/bo carry no raw-value fields,
/// so C passes a NULL value pointer).
const BI_SEVERITY_FIELDS: [&str; 2] = ["ZSV", "OSV"];
/// C `MAX_ENUM_STRING_SIZE` (devAsynInt32.c:66): enum strings are capped
/// at 25 chars + NUL when copied into the record's DBF_STRING slots.
const MAX_ENUM_STRING_LEN: usize = 25;

/// The state-field family an enum record exposes — the C `initMbbi` vs
/// `initBi` distinction. `Mbb` covers mbbi/mbbo (16 states, ZRST/ZRVL/ZRSV…);
/// `Bi` covers bi/bo (2 states, ZNAM/ONAM + ZSV/OSV, no raw-value fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumRecordShape {
    Mbb,
    Bi,
}

impl EnumRecordShape {
    /// Discriminate by the state fields the record exposes (mirroring C's
    /// per-record init: a `ZRST` field => mbbi/mbbo, a `ZNAM` field =>
    /// bi/bo). `None` for any record that is not an enum state record.
    fn of_record(record: &dyn Record) -> Option<Self> {
        if record.get_field("ZRST").is_some() {
            Some(Self::Mbb)
        } else if record.get_field("ZNAM").is_some() {
            Some(Self::Bi)
        } else {
            None
        }
    }

    /// (string-fields, optional value-fields, severity-fields) for this
    /// shape. bi/bo carry no raw-value fields (C passes a NULL value ptr).
    #[allow(clippy::type_complexity)]
    fn fields(
        self,
    ) -> (
        &'static [&'static str],
        Option<&'static [&'static str]>,
        &'static [&'static str],
    ) {
        match self {
            Self::Mbb => (
                &MBB_STRING_FIELDS,
                Some(&MBB_VALUE_FIELDS),
                &MBB_SEVERITY_FIELDS,
            ),
            Self::Bi => (&BI_STRING_FIELDS, None, &BI_SEVERITY_FIELDS),
        }
    }
}

/// The status and `udf` write of the C asyn `init*` function for this record
/// type and interface, given whether its initial read succeeded.
fn init_status(record_type: &str, iface_type: &str, read_back: bool) -> DeviceInitOutcome {
    use epics_base_rs::server::device_support::DeviceUdf::{Defined, Untouched};
    match (record_type, iface_type) {
        // initAo (devAsynInt32.c:955-959), initBo, initMbbo: 0 lets the record
        // convert the RVAL read back, 2 otherwise.
        ("ao", "asynInt32") | ("bo" | "mbbo", _) if read_back => DeviceInitOutcome::ok(),
        // devAsynFloat64.c initAo sets VAL and `udf = 0`; initMbboDirect sets
        // VAL, B0-B15 and `udf = FALSE`; devAsynInt64.c initAo sets VAL only.
        ("ao", "asynFloat64") | ("mbboDirect", _) if read_back => {
            DeviceInitOutcome::DoNotConvert(Defined)
        }
        ("ao" | "bo" | "mbbo" | "mbboDirect", _) => DeviceInitOutcome::DoNotConvert(Untouched),
        // initLo/initLLo and devAsynOctet.c initCommon: `udf = 0`, INIT_OK.
        _ if read_back => DeviceInitOutcome::Ok(Defined),
        _ => DeviceInitOutcome::ok(),
    }
}

/// Build the `(field, value)` list C `setEnums` (devAsynInt32.c:415-435)
/// writes for `entries` onto a record of the given `shape` — the single
/// producer shared by init (apply directly to the record) and the runtime
/// callback (post DBE_PROPERTY). Faithful to `setEnums`: every output slot
/// up to the record's state count is first cleared (empty string / value 0
/// / severity 0), then the driver's entries fill slots `0..numIn`, so a
/// driver advertising fewer states blanks the record's surplus `.db`
/// strings. Strings are truncated to MAX_ENUM_STRING_SIZE-1.
fn enum_table_fields(
    shape: EnumRecordShape,
    entries: &[crate::param::EnumEntry],
) -> Vec<(String, EpicsValue)> {
    let (strings, values, severities) = shape.fields();
    let mut out = Vec::with_capacity(strings.len() * 3);
    for i in 0..strings.len() {
        let entry = entries.get(i);
        let mut s = entry.map(|e| e.string.clone()).unwrap_or_default();
        // C `setEnums` truncates to MAX_ENUM_STRING_SIZE-1 (byte memcpy);
        // EPICS state strings are ASCII so a char boundary at 25 is safe.
        if s.chars().count() > MAX_ENUM_STRING_LEN {
            s = s.chars().take(MAX_ENUM_STRING_LEN).collect();
        }
        out.push((strings[i].to_string(), EpicsValue::String(s.into())));
        if let Some(value_fields) = values {
            let value = entry.map(|e| e.value).unwrap_or(0);
            out.push((value_fields[i].to_string(), EpicsValue::ULong(value as u32)));
        }
        let severity = entry.map(|e| e.severity).unwrap_or(0);
        out.push((
            severities[i].to_string(),
            EpicsValue::Short(severity as i16),
        ));
    }
    out
}

impl AsynDeviceSupport {
    /// C `devAsynInt32.c::initAi` (lines 821-828): the `@asynMask` nbits
    /// bounds when given, else the driver's `getBounds`, which a failure
    /// leaves at (0, 0).
    fn init_device_bounds(&mut self) {
        self.device_bounds = if let Some(m) = self.int32_mask {
            (m.device_low.into(), m.device_high.into())
        } else {
            let op = if self.iface_type == "asynInt64" {
                RequestOp::GetBoundsInt64
            } else {
                RequestOp::GetBoundsInt32
            };
            let user = self.init_user();
            self.handle
                .submit_blocking(op, user)
                .ok()
                .and_then(|r| r.bounds)
                .unwrap_or((0, 0))
        };
    }

    /// C `convertAi`/`convertAo` (devAsynInt32.c:437-473) after pass 0:
    /// `ESLO = (EGUF - EGUL) / (high - low)` and
    /// `EOFF = (high*EGUL - low*EGUF) / (high - low)` over the init bounds.
    fn apply_linear_eslo_eoff(&self, record: &mut dyn Record) {
        let (low, high) = self.device_bounds;
        if low == high {
            return;
        }
        let (low, high) = (low as f64, high as f64);
        let eguf = match record.get_field("EGUF") {
            Some(EpicsValue::Double(v)) => v,
            _ => return,
        };
        let egul = match record.get_field("EGUL") {
            Some(EpicsValue::Double(v)) => v,
            _ => return,
        };
        let denom = high - low;
        let eslo = (eguf - egul) / denom;
        let eoff = (high * egul - low * eguf) / denom;
        let _ = record.put_field("ESLO", EpicsValue::Double(eslo));
        let _ = record.put_field("EOFF", EpicsValue::Double(eoff));
    }

    /// Build a `RequestOp` for reading the current interface type; an array
    /// read carries `record`'s buffer.
    fn read_op(&self, record: &dyn Record) -> Option<RequestOp> {
        match self.iface_type.as_str() {
            "asynInt32" => Some(RequestOp::Int32Read),
            "asynInt64" => Some(RequestOp::Int64Read),
            "asynFloat64" => Some(RequestOp::Float64Read),
            // asynOctet: a plain read, OR a write-then-read when a literal
            // command is cached (asynOctetCmdResponse, C `callbackSiCmdResponse`
            // devAsynOctet.c:853-855 — write the command, then read the reply
            // into VAL). flush=false: C does plain writeIt → readIt on the raw
            // asynOctet interface with NO pre-flush, so the reply includes any
            // bytes already on the warm line (unlike asynOctetSyncIO::writeRead).
            "asynOctet" => Some(match &self.octet_cmd {
                Some(cmd) => RequestOp::OctetWriteRead {
                    data: {
                        let mut data = self.recycled();
                        data.extend_from_slice(cmd);
                        data
                    },
                    buf_size: self.octet_max_size,
                    flush: false,
                    entry: ReadEntry::new(),
                },
                // asynOctetWriteRead: the command is read where the request runs.
                None if self.device_job.write_read.get().is_some() => RequestOp::OctetWriteRead {
                    data: self.recycled(),
                    buf_size: self.octet_max_size,
                    flush: false,
                    entry: ReadEntry::new(),
                },
                None => RequestOp::OctetRead {
                    buf_size: self.octet_max_size,
                    entry: ReadEntry::new(),
                },
            }),
            "asynUInt32Digital" => Some(RequestOp::UInt32DigitalRead { mask: self.mask }),
            "asynEnum" => Some(RequestOp::EnumRead),
            "asynInt8Array" => Some(RequestOp::Int8ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), i8),
                max_elements: self.max_array_elements,
            }),
            "asynInt16Array" => Some(RequestOp::Int16ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), i16),
                max_elements: self.max_array_elements,
            }),
            "asynInt32Array" => Some(RequestOp::Int32ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), i32),
                max_elements: self.max_array_elements,
            }),
            "asynInt64Array" => Some(RequestOp::Int64ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), i64),
                max_elements: self.max_array_elements,
            }),
            "asynFloat32Array" => Some(RequestOp::Float32ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), f32),
                max_elements: self.max_array_elements,
            }),
            "asynFloat64Array" => Some(RequestOp::Float64ArrayRead {
                buf: record_read_buf!(record.get_field("VAL"), f64),
                max_elements: self.max_array_elements,
            }),
            _ => None,
        }
    }

    /// Apply the asynInt32 `@asynMask` mask + sign-extend to a raw read
    /// value (C `processCallbackInput`, devAsynInt32.c:485-488). A no-op
    /// when no nbits was configured (`int32_mask == None`).
    fn apply_int32_mask(&self, value: i32) -> i32 {
        self.int32_mask.map_or(value, |m| m.apply(value))
    }

    /// Derive the record's value from an interrupt sample **through the interface
    /// the record is bound on** — the I/O-Intr twin of the polled
    /// [`AsynDeviceSupport::result_to_value`], which reads the driver's reply
    /// through that same interface (`asynFloat64` takes `float_val`, `asynInt32`
    /// takes `int_val`, …).
    ///
    /// The scalar rules are the interface read rules themselves ([`crate::param::ParamValue::as_int32`]
    /// and friends), the very ones [`crate::param::ParamList::get_int32`] applies when the
    /// polled path reads the same parameter — one owner, so the two paths cannot
    /// disagree about what an `asynFloat64` record may be handed.
    ///
    /// A sample the record's interface cannot read is an [`AsynError::TypeMismatch`],
    /// exactly as it is on the polled path, and the caller alarms the record.
    /// Coercing it instead (what a variant-directed mapping does) hands the record a
    /// value of the wrong `EpicsValue` kind, which then misses its conversion arm in
    /// [`AsynDeviceSupport::store_read_value`] and lands on raw `set_val` — silently
    /// bypassing ai ASLO/AOFF/SMOO, asynInt32 ai ESLO/EOFF, and bi/mbbi RVAL state
    /// tables. C cannot reach that state at all: it keeps one interrupt list per
    /// interface, so a record is only ever handed a value of its own type.
    ///
    /// Arrays keep the per-interface `convert` of [`convert_param_array_to_iface`]
    /// (C fires all six array interfaces, each with its own converted copy).
    /// `Ok(None)` for an interface with no value mapping (e.g. `asynGenericPointer`),
    /// which stores nothing — as before.
    ///
    /// Takes `&self` for the same reason `result_to_value` does: `asynInt32` reads
    /// are masked and sign-extended by the record's `@asynMask` nbits, and C applies
    /// that in the interrupt path too (`interruptCallbackInput`, devAsynInt32.c:537-539,
    /// the same `value &= mask` + bipolar sign-extend as `processCallbackInput` at
    /// :485-488). A free function could not reach `int32_mask`, and the two paths
    /// would silently disagree again — this time about the mask instead of the type.
    ///
    /// `octet_nul` is the fire's
    /// [`crate::interrupt::InterruptValue::octet_counts_nul`]: C hands an
    /// asynOctet user the text and its NUL.
    fn param_value_for_iface(
        &self,
        pv: &crate::param::ParamValue,
        octet_nul: bool,
    ) -> Result<Option<EpicsValue>, AsynError> {
        let iface_type = self.iface_type.as_str();
        if let Some(arr) = convert_param_array_to_iface(iface_type, pv) {
            return Ok(Some(arr));
        }
        let val = match iface_type {
            "asynInt32" => EpicsValue::Long(self.apply_int32_mask(pv.as_int32()?)),
            "asynInt64" => EpicsValue::Int64(pv.as_int64()?),
            "asynFloat64" => EpicsValue::Double(pv.as_float64()?),
            "asynOctet" => {
                let bytes = pv.as_octet()?;
                EpicsValue::String(if octet_nul {
                    let text = &bytes[..bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len())];
                    epics_base_rs::types::PvString::from_bytes([text, &[0]].concat())
                } else {
                    bytes.into()
                })
            }
            "asynUInt32Digital" => EpicsValue::Long(pv.as_uint32()? as i32),
            "asynEnum" => EpicsValue::Enum(pv.as_enum()?.0 as u16),
            // An array interface reached here only because the sample is not an
            // array (`convert_param_array_to_iface` handles every array sample):
            // the record's interface cannot read it, same as the scalar mismatches.
            "asynInt8Array" | "asynInt16Array" | "asynInt32Array" | "asynInt64Array"
            | "asynFloat32Array" | "asynFloat64Array" => {
                return Err(AsynError::TypeMismatch {
                    expected: "Array",
                    actual: pv.type_name(),
                });
            }
            _ => return Ok(None),
        };
        Ok(Some(val))
    }

    fn octet_value(&self, d: &[u8]) -> EpicsValue {
        let n = d.len().min(self.octet_read_cap());
        EpicsValue::String(epics_base_rs::types::PvString::from_bytes(&d[..n]))
    }

    /// Extract an EpicsValue from a RequestResult based on interface type.
    fn result_to_value(&self, result: &RequestResult) -> Option<EpicsValue> {
        match self.iface_type.as_str() {
            "asynInt32" => result.int_val.map(EpicsValue::Long),
            "asynInt64" => result.int64_val.map(EpicsValue::Int64),
            "asynFloat64" => result.float_val.map(EpicsValue::Double),
            "asynOctet" => result
                .octet()
                .filter(|read| read.status().is_ok())
                .map(|read| self.octet_value(read.data())),
            "asynUInt32Digital" => result.uint_val.map(|v| EpicsValue::Long(v as i32)),
            "asynEnum" => result.enum_index.map(|v| EpicsValue::Enum(v as u16)),
            // One copy each, straight into the shared buffer: cloning the
            // `Vec` first would copy it a second time on the way in.
            // The driver's read buffer becomes the record's: same element
            // type shares it, a cast (i8 -> u8, i64 -> i32) copies once.
            "asynInt8Array" => result
                .int8_array
                .as_ref()
                .map(|v| EpicsValue::CharArray(v.iter().map(|&x| x as u8).collect())),
            "asynInt16Array" => result.int16_array.clone().map(EpicsValue::ShortArray),
            "asynInt32Array" => result.int32_array.clone().map(EpicsValue::LongArray),
            "asynInt64Array" => result
                .int64_array
                .as_ref()
                .map(|v| EpicsValue::LongArray(v.iter().map(|&x| x as i32).collect())),
            "asynFloat32Array" => result.float32_array.clone().map(EpicsValue::FloatArray),
            "asynFloat64Array" => result.float64_array.clone().map(EpicsValue::DoubleArray),
            _ => None,
        }
    }

    /// Truncate an octet **write** payload to the per-record length cap, mirroring
    /// C `writeOctet`'s `getStringLen(pasynUser, maxChars)` = `min(maxChars,
    /// drvUser->len)` (drvModbusAsyn.cpp:1521/1524/1531). The cap is the raw
    /// per-record LEN (`octet_len_cap`; `None` ⇒ C `len == -1` ⇒ uncapped). SIZV
    /// does NOT bound writes — C's `maxChars` is the actual data length, not the
    /// record buffer — so this uses `octet_len_cap`, not `octet_max_size`.
    fn cap_octet_write(&self, mut data: Vec<u8>) -> Vec<u8> {
        if let Some(cap) = self.octet_len_cap {
            data.truncate(cap);
        }
        data
    }

    /// Truncate a delivered octet **read** value to the per-record buffer length,
    /// mirroring C's I/O-Intr poller `getStringLen(pasynUser, sizeof(stringBuffer))`
    /// = `min(buffer, drvUser->len)` (drvModbusAsyn.cpp:1914). The polled read caps
    /// at the driver via `buf_size`, but the I/O-Intr ring fires the driver's full
    /// block (the poller is per-reason, shared by records with different LEN), so the
    /// cap is applied here, consumer-side, where the per-record `octet_max_size`
    /// (= min(SIZV, LEN)) lives. asynOctet reads are delivered as `EpicsValue::String`
    /// (result_to_value / param_value_to_epics_value); only that variant is capped,
    /// leaving array-interface `CharArray` reads (bounded by `max_array_elements`)
    /// untouched. Bytes are truncated verbatim — `PvString` is raw bytes, matching the
    /// polled path's `&d[..n]` slice.
    fn cap_octet_read_value(&self, val: EpicsValue) -> EpicsValue {
        let cap = self.octet_read_cap();
        match val {
            EpicsValue::String(s) if s.len() > cap => EpicsValue::String(
                epics_base_rs::types::PvString::from_bytes(&s.as_bytes()[..cap]),
            ),
            other => other,
        }
    }

    /// The most bytes a read hands the record: C `if (nBytesRead == valSize)
    /// nBytesRead--` (callbackWfRead and kin) and `if (len >= valSize) len =
    /// valSize-1` (interruptCallback), within the request size.
    fn octet_read_cap(&self) -> usize {
        self.octet_max_size
            .min(self.octet_val_size.saturating_sub(1))
    }

    /// Build a `RequestOp` for writing an `EpicsValue` for the current interface type.
    /// asynOctetWriteBinary: build the write op for exactly NORD bytes, INCLUDING
    /// any interior NUL. C `callbackWfWriteBinary` (devAsynOctet.c:1086-1091) does
    /// `writeIt(pasynUser, pwf->bptr, pwf->nord)` — no `my_strnlen` NUL-trim
    /// (contrast `callbackWfWrite`, :1071-1076, which trims). A plain `OctetWrite`
    /// is emitted, NOT `OctetWriteBinary`: devAsynOctet does NO output-EOS
    /// suppression, so the port's configured OEOS is still appended. (The name
    /// collides with asynRecord OFMT=Binary, which DOES clear OEOS — a different
    /// dset with different semantics; routing here to `OctetWriteBinary` would
    /// wrongly strip the terminator.)
    fn binary_write_op(&self, record: &dyn Record, val: &EpicsValue) -> Option<RequestOp> {
        let EpicsValue::CharArray(data) = val else {
            // Not a CHAR waveform value (not expected for this dset): fall back to
            // the text path rather than silently dropping the write.
            return self.write_op(val);
        };
        // Size by the record's NORD (C `pwf->nord`), clamped to the value length.
        // NORD is DBF_ULONG on waveform/aai/aao and DBF_LONG on subArray, so read
        // it through the numeric view rather than one variant.
        let nord = match record.get_field("NORD").and_then(|v| v.to_f64()) {
            Some(n) => (n.max(0.0) as usize).min(data.len()),
            None => data.len(),
        };
        let mut bytes = self.recycled();
        bytes.extend_from_slice(&data[..nord]);
        Some(RequestOp::OctetWrite {
            data: self.cap_octet_write(bytes),
        })
    }

    /// Every text octet write — stringout, lso, printf, scalcout, the
    /// `asynOctetWrite` waveform — sends `my_strnlen(val, size)` bytes
    /// (devAsynOctet.c:943-958, 1076, 1214, 1229, 1256), so no NUL a VAL holds
    /// reaches the driver. Only [`Self::binary_write_op`] sends NULs.
    fn text_octet_write(&self, bytes: &[u8]) -> RequestOp {
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let mut data = self.recycled();
        data.extend_from_slice(&bytes[..len]);
        RequestOp::OctetWrite {
            data: self.cap_octet_write(data),
        }
    }

    /// The write of `val` through this record's interface; a data-carrying op
    /// refills the spare's buffer.
    fn write_op(&self, val: &EpicsValue) -> Option<RequestOp> {
        // First try exact match, then coerce numeric types to match the interface.
        // C EPICS always converts record VAL to the interface type (e.g. ao→double).
        match (self.iface_type.as_str(), val) {
            ("asynInt32", EpicsValue::Long(v)) => Some(RequestOp::Int32Write { value: *v }),
            ("asynInt32", EpicsValue::Enum(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt32", EpicsValue::Short(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt32", EpicsValue::Double(v)) => {
                Some(RequestOp::Int32Write { value: *v as i32 })
            }
            ("asynInt32", EpicsValue::Float(v)) => Some(RequestOp::Int32Write { value: *v as i32 }),
            ("asynInt64", EpicsValue::Int64(v)) => Some(RequestOp::Int64Write { value: *v }),
            ("asynInt64", EpicsValue::Long(v)) => Some(RequestOp::Int64Write { value: *v as i64 }),
            ("asynInt64", EpicsValue::Double(v)) => {
                Some(RequestOp::Int64Write { value: *v as i64 })
            }
            ("asynFloat64", EpicsValue::Double(v)) => Some(RequestOp::Float64Write { value: *v }),
            ("asynFloat64", EpicsValue::Long(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Float(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Short(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynFloat64", EpicsValue::Enum(v)) => {
                Some(RequestOp::Float64Write { value: *v as f64 })
            }
            ("asynOctet", EpicsValue::String(s)) => Some(self.text_octet_write(s.as_bytes())),
            ("asynOctet", EpicsValue::CharArray(data)) => Some(self.text_octet_write(data)),
            // Coerce numeric types to octet (e.g. longout writing to NDArrayPort string param)
            ("asynOctet", v) => {
                // A number's text holds no NUL, so the strnlen cut is moot.
                use std::io::Write as _;
                let mut bytes = self.recycled();
                let _ = write!(bytes, "{v}");
                Some(RequestOp::OctetWrite {
                    data: self.cap_octet_write(bytes),
                })
            }
            ("asynUInt32Digital", EpicsValue::Long(v)) => Some(RequestOp::UInt32DigitalWrite {
                value: *v as u32,
                mask: self.mask,
            }),
            ("asynUInt32Digital", EpicsValue::Enum(v)) => Some(RequestOp::UInt32DigitalWrite {
                value: *v as u32,
                mask: self.mask,
            }),
            ("asynEnum", EpicsValue::Long(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
            ("asynEnum", EpicsValue::Enum(v)) => Some(RequestOp::EnumWrite { index: *v as usize }),
            ("asynInt8Array", EpicsValue::CharArray(data)) => {
                let mut buf = self.recycled();
                buf.extend(data.iter().map(|&x| x as i8));
                Some(RequestOp::Int8ArrayWrite { data: buf })
            }
            ("asynInt16Array", EpicsValue::ShortArray(data)) => {
                let mut buf = self.recycled();
                buf.extend_from_slice(data);
                Some(RequestOp::Int16ArrayWrite { data: buf })
            }
            ("asynInt32Array", EpicsValue::LongArray(data)) => {
                let mut buf = self.recycled();
                buf.extend_from_slice(data);
                Some(RequestOp::Int32ArrayWrite { data: buf })
            }
            ("asynInt64Array", EpicsValue::LongArray(data)) => {
                let mut buf = self.recycled();
                buf.extend(data.iter().map(|&x| x as i64));
                Some(RequestOp::Int64ArrayWrite { data: buf })
            }
            ("asynFloat32Array", EpicsValue::FloatArray(data)) => {
                let mut buf = self.recycled();
                buf.extend_from_slice(data);
                Some(RequestOp::Float32ArrayWrite { data: buf })
            }
            ("asynFloat64Array", EpicsValue::DoubleArray(data)) => {
                let mut buf = self.recycled();
                buf.extend_from_slice(data);
                Some(RequestOp::Float64ArrayWrite { data: buf })
            }
            _ => None,
        }
    }

    /// C `asynPrint(pPvt->pasynUser, …)` from device support: the record's
    /// user is connected to this port and address, so the print resolves the
    /// device's trace config (`findTracePvt`, asynManager.c:548-553).
    fn dev_print(
        &self,
        mask: crate::trace::TraceMask,
        file: &str,
        line: u32,
        args: std::fmt::Arguments<'_>,
    ) {
        self.handle
            .user_trace()
            .print(self.addr, self.reason as i32, mask, file, line, args);
    }

    /// The prints of C's ring pop (`getCallbackValue`, `getRingBufferValue`)
    /// and of the latest slot's process (devAsynOctet.c:777-783,
    /// devAsynXXXArray.cpp:302-309), for a delivery taken from the store.
    fn popped(&self, d: &Delivery) {
        use crate::trace::TraceMask;
        let warning = TraceMask::WARNING;
        let (record, driver) = (&self.record_name, self.dset.driver());
        if d.overflows > 0 {
            // devAsynXXXArray.cpp:378's functionName ends in a space.
            let (func, kind) = match self.dset {
                Dset::Octet => ("getRingBufferValue", "warning"),
                Dset::Array => ("getRingBufferValue ", "error"),
                _ => ("getCallbackValue", "warning"),
            };
            self.dev_print(
                warning,
                file!(),
                line!(),
                format_args!(
                    "{record} {driver}::{func} {kind}, {} ring buffer overflows\n",
                    d.overflows
                ),
            );
        }
        if self.dset.is_scalar() {
            // devAsynUInt32Digital.c:619 prints its epicsUInt32 with %d.
            let value = match Scalar::delivered(&d.entry.value) {
                Scalar::UInt(u) => Scalar::Int(u as i32 as i64),
                value => value,
            };
            self.dev_print(
                TraceMask::IO_DEVICE,
                file!(),
                line!(),
                format_args!("{record} {driver}::getCallbackValue from ringBuffer value={value}\n"),
            );
        }
        let func = match self.dset {
            Dset::Octet => "processCommon",
            Dset::Array => "process,",
            // C has no scalar latest slot: its ring of 0 never scans.
            _ => return,
        };
        if d.backlog {
            self.dev_print(
                warning,
                file!(),
                line!(),
                format_args!(
                    "{record} {driver}::{func} warning: multiple interrupt callbacks between processing\n"
                ),
            );
        }
    }

    /// What the record holds once a callback's process stored it, as C dumps
    /// it at ASYN_TRACEIO_DEVICE: `strlen(pValue)` bytes for devAsynOctet
    /// (devAsynOctet.c:799-803), `nord` elements for devAsynXXXArray
    /// (devAsynXXXArray.cpp:321-324, whose functionName slot gets driverName).
    fn print_callback_value(&self, record: &dyn Record) {
        let io = crate::trace::TraceMask::IO_DEVICE;
        let t = self.handle.user_trace();
        if !t.manager.is_enabled_device(&t.port, self.addr, io) {
            return;
        }
        let rec = &self.record_name;
        let val = record.get_field("VAL");
        let (data, label) = if self.dset == Dset::Octet {
            let bytes = match &val {
                Some(EpicsValue::String(s)) => s.as_bytes(),
                Some(EpicsValue::CharArray(a) | EpicsValue::UCharArray(a)) => &a[..],
                _ => &[],
            };
            let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            let label = format!("{rec} devAsynOctet::processCommon len={len},  data:");
            (bytes[..len].to_vec(), label)
        } else {
            let nord = record
                .get_field("NORD")
                .and_then(|v| v.to_f64())
                .map_or(0, |n| n.max(0.0) as usize);
            let data = val.map_or_else(Vec::new, |v| record_array_bytes(&v, nord));
            let label =
                format!("{rec} devAsynXXXArray::devAsynXXXArray nord={nord}, pRecord_->bptr data:");
            (data, label)
        };
        t.manager.output_device_io(
            &t.port,
            Some(self.addr),
            self.reason as i32,
            io,
            &data,
            &label,
            file!(),
            line!(),
        );
    }

    /// The C dset this record's DTYP bound. A time series keeps only its base
    /// interface, so the suffix is put back.
    /// C `registerInterruptUser(interruptCallback)` (devAsynXXXTimeSeries.h:
    /// 158-160): the accumulating callback, registered by `process` on BUSY
    /// 0->1 and cancelled, by dropping it, on BUSY 1->0.
    fn time_series_subscribe(
        &self,
        ts: Arc<TimeSeriesState>,
    ) -> Option<crate::interrupt::SyncCallbackSubscription> {
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.handle.param_list(self.addr)),
            user_addr: Some(self.addr),
            drv_user: self.drv_user.clone(),
            uint32_mask: None,
            // Per-interface routing: a driver that fires interface-typed
            // values (e.g. Modbus) must reach this accumulating callback
            // with only this record's interface, or a single poll's several
            // typed fires would each be accumulated as separate samples.
            iface: self.iface,
        };
        let tx = self.time_series_wake.clone();
        let trace = self.handle.user_trace();
        let (addr, reason) = (self.addr, self.reason as i32);
        let (record, driver) = (self.record_name.clone(), self.dset.driver());
        self.handle
            .interrupts()
            .register_sync_callback(filter, move |iv| {
                let mut st = ts.inner.lock().unwrap();
                let nord = st.buf.as_ref().map_or(0, TsBuf::len);
                trace.print(
                    addr,
                    reason,
                    crate::trace::TraceMask::IO_DEVICE,
                    file!(),
                    line!(),
                    format_args!(
                        "{record} {driver}::interruptCallback, value={}, nord={nord}\n",
                        CFloat(TsBuf::sample(&iv.value))
                    ),
                );
                // Not acquiring (C `if (pPvt->busy)` guard): nothing to do.
                if !st.busy {
                    return;
                }
                let nelm = st.nelm;
                // Append while room remains (C `if (pPvt->nord < pwf->nelm)`).
                // A None buffer (FTVL invalid) or a full buffer returns.
                let full = match st.buf.as_mut() {
                    Some(buf) if buf.len() < nelm => {
                        buf.push(&iv.value);
                        buf.len() >= nelm
                    }
                    _ => return,
                };
                // Transport status: C updates `pPvt->status = pasynUser->
                // auxStatus` on EVERY callback, including one between the
                // buffer filling and the process that cancels it
                // (devAsynXXXTimeSeries.h:213 is outside the `if(busy)`).
                // Here it is reached only while acquiring (the not-busy guard
                // returned above) — a deliberate divergence: that sample's
                // error does not leak into the next acquisition's alarm. Within acquisition the behaviour is C's:
                // keep the first non-success status until the next process.
                if st.status == crate::error::AsynStatus::Success
                    && iv.aux_status != crate::error::AsynStatus::Success
                {
                    st.status = iv.aux_status;
                }
                // Completion. DIVERGENCE from C: C completes LAZILY — the
                // sample that fills the last slot is stored and acquisition
                // stays busy; the NEXT sample finds `nord == nelm`, is
                // discarded, and only then clears busy + processes
                // (devAsynXXXTimeSeries.h:203-211). That delays completion by
                // one sample and hangs busy=1 forever if the stream stops at
                // exactly NELM. This port completes EAGERLY on the filling
                // sample: identical stored data (the first NELM samples; the
                // would-be C trigger sample is dropped by the not-busy guard
                // either way) but deterministic completion. try_send: the
                // callback runs inline in the driver notify and must not block;
                // a full wakeup channel means a process is already pending.
                if full {
                    st.busy = false;
                    drop(st);
                    if let Some(tx) = &tx {
                        let _ = tx.try_send(());
                    }
                }
            })
            .inspect_err(|e| self.report_register_refusal(Register::TimeSeries, e))
            .ok()
    }

    fn bound_dset(&self, record_type: &str) -> Dset {
        if self.time_series.is_some() {
            Dset::of(record_type, &format!("{}TimeSeries", self.iface_type))
        } else {
            Dset::of(record_type, &self.iface_type)
        }
    }

    /// C's `queueRequest` on a port that cannot block: the callback runs
    /// inside it, so the request's lines print before it returns.
    fn submit_traced(&mut self, op: RequestOp) -> AsynResult<RequestResult> {
        let trace = self.handle.user_trace();
        // Only an octet write prints its buffer, and only when traced.
        let keep = !carries_buffer(&op)
            || self.dset == Dset::Octet
                && trace.manager.is_enabled_device(
                    &trace.port,
                    self.addr,
                    crate::trace::TraceMask::IO_DEVICE,
                );
        let kept = keep.then(|| op.clone());
        let sent = kept.as_ref().map_or_else(|| Sent::bare(&op), Sent::of);
        let mut ran = self.handle.submit_blocking(op, self.process_user());
        if let Ok(result) = &mut ran {
            Int32Mask::apply_to_read(self.int32_mask, result);
        }
        sent.raise_short_write(self.dset, &mut ran);
        // The callback ran inside `queueRequest`, before `processXxx` reports
        // its status (devAsynInt32.c:838-841).
        if !ran.as_ref().is_err_and(AsynError::is_queue_refused) {
            ProcessLine {
                trace: &trace,
                record: &self.record_name,
                file: self.dset,
                addr: self.addr,
                reason: self.reason as i32,
            }
            .finished(&self.device_job.last_status, &sent, ran.as_ref());
        }
        self.report_queued(ran.as_ref().err());
        ran
    }

    /// [`Self::submit_traced`] for a read. The callback runs under this
    /// record's hold, so `asynOctetWriteRead` asks a self-named command record
    /// through `record`.
    fn submit_read(&mut self, mut op: RequestOp, record: &dyn Record) -> AsynResult<RequestResult> {
        if let Some(src) = self.device_job.write_read.get() {
            if !src.fill(&mut op, Some((self.record_name.as_bytes(), record))) {
                let trace = self.handle.user_trace();
                let ran = src.failed(Some(&ProcessLine {
                    trace: &trace,
                    record: &self.record_name,
                    file: Dset::Octet,
                    addr: self.addr,
                    reason: self.reason as i32,
                }));
                self.report_queued(None);
                return ran;
            }
        }
        self.submit_traced(op)
    }

    /// The `reportQueueRequestStatus` a process owes once its request ran
    /// (`error` is `None` or not a refusal) or was refused.
    fn report_queued(&mut self, error: Option<&AsynError>) {
        match error {
            Some(e) if e.is_queue_refused() => {
                self.report_queue_status(e.status(), &e.message_display());
            }
            _ => {
                if !self.handle.can_block() {
                    self.report_queue_status(AsynStatus::Success, &"");
                }
            }
        }
    }

    /// The RVAL C's dset stores from a raw readback. `asynUInt32Digital`
    /// (`processBo`, `processMbbiDirect`, …) and the `asynInt32` mbbi/mbbo
    /// (devAsynInt32.c:1270, :1296, :1311) AND it with the record's MASK; the
    /// `asynInt32` bi/bo/ao store it as read (:1162, :1187, :1202, :956).
    fn dset_rval(&self, record: &dyn Record, raw: i32) -> i32 {
        let masks = self.iface_type == "asynUInt32Digital"
            || (self.iface_type == "asynInt32" && record.get_field("NOBT").is_some());
        match record.get_field("MASK") {
            Some(EpicsValue::ULong(mask)) if masks => raw & mask as i32,
            _ => raw,
        }
    }

    /// The one synchronous write. Both `write()` and the non-blocking branch
    /// of `write_begin` end here, so the trace line and the alarm mapping
    /// cannot differ between them.
    /// C processCommon's alarm for an output transfer: its own, or the failed
    /// status mapped with the dset's default — WRITE_ALARM (devAsynInt32.c:1009,
    /// devAsynOctet.c:806-809), but READ_ALARM in the array dsets' one
    /// `process()` (devAsynXXXArray.cpp:330-332).
    fn output_alarm(&self, wrote: Result<(u16, u16), AsynStatus>) -> (u16, u16) {
        use epics_base_rs::server::recgbl::alarm_status;
        let default_stat = if self.iface_type.ends_with("Array") {
            alarm_status::READ_ALARM
        } else {
            alarm_status::WRITE_ALARM
        };
        wrote.unwrap_or_else(|status| asyn_status_to_alarm_with_default(status, default_stat))
    }

    /// Whether `processCommon` never sees `e`: only the scalar dsets keep a
    /// refused `queueRequest` in `result.status` (devAsynInt32.c:796).
    /// devAsynOctet.c:719 and devAsynXXXArray.cpp:266 only report it, so their
    /// process maps the asynSuccess the last one reset, which leaves the last
    /// transfer's `result.alarmStatus` standing (asynEpicsUtils.c:238-240):
    /// the alarm stays as the last transfer left it.
    fn drop_refusal(&self, e: &AsynError) -> bool {
        e.is_queue_refused() && matches!(self.dset, Dset::Octet | Dset::Array)
    }

    /// devAsynOctet's `processCommon` defines the record on every pass whose
    /// result is success (devAsynOctet.c:817-818), outputs included; a refused
    /// queue leaves that status at success. The other devAsyn outputs leave
    /// UDF to the record.
    fn define_on_write(&self, common: &mut CommonFields, landed: bool) {
        if self.output && self.dset == Dset::Octet && landed {
            common.udf = 0;
        }
    }

    fn write_now(&mut self, op: RequestOp) -> CaResult<()> {
        let ran = self.submit_traced(op);
        if let Err(e) = &ran {
            if self.drop_refusal(e) {
                return Ok(());
            }
        }
        (self.last_alarm_status, self.last_alarm_severity) = self.output_alarm(write_alarm(&ran));
        ran.map(|_| ()).map_err(asyn_to_ca_error)
    }

    /// Store a freshly-read scalar value into the record and report whether
    /// the record's built-in RVAL→VAL conversion should be **skipped**
    /// (`true`) or **run** (`false`). This mirrors C device support's
    /// `processXxx` return value:
    ///
    /// - `asynFloat64` ai (devAsynFloat64.c:594-604): the device support
    ///   computes the engineering VAL itself — ASLO/AOFF then the SMOO filter
    ///   — and returns RTN_DO_NOT_CONVERT (`2`). Here: set VAL via
    ///   [`Self::apply_float64_ai_conversion`], return `true`. Gated on the
    ///   record exposing `SMOO` (the ai-only field) so an `ao` readback — which
    ///   has ASLO/AOFF but no SMOO and a different (output) conversion — is not
    ///   disturbed.
    /// - `asynInt32` ai (devAsynInt32.c:848-851): the device support sets
    ///   `pr->rval = value` and returns `0`, letting the ai record's `convert()`
    ///   apply ROFF/ASLO/AOFF, the LINR linearization (ESLO/EOFF), and SMOO.
    ///   Here: route the raw int through RVAL, return `false`. Gated on the
    ///   record exposing `ESLO` (the ai linearization field, same discriminator
    ///   as the init-time [`Self::apply_linear_eslo_eoff`]); without it the raw
    ///   counts reached VAL straight and the computed ESLO/EOFF were dead.
    /// - records (`asynInt32` or `asynUInt32Digital`) with a non-trivial
    ///   `raw -> VAL` convert claim the raw via `apply_raw_readback` (`true`):
    ///   it sets both RVAL and VAL the way C's `processXxx` does, and returning
    ///   `true` (computed) makes the framework skip the record's forward
    ///   convert, which would otherwise recompute RVAL from the stale VAL and
    ///   discard the readback. Outputs: ao (engineering inverse), mbbo/bo/
    ///   mbboDirect (state table / 0-1 / bit map). Inputs: bi (0/1 map,
    ///   `processBi`), mbbi (shift/state index, `processMbbi`), mbbiDirect
    ///   (shift/bits, `processMbbiDirect`), each given the RVAL from
    ///   [`Self::dset_rval`]. The hook is the *device-distinct*
    ///   entry — the Soft Channel path stays on `set_val`, so it does not
    ///   collide with a soft-link value write.
    /// - everything else (longin/longout): set VAL directly, return `true`
    ///   (device support produced the final value, as before) — longin/longout
    ///   VAL *is* the raw (no convert).
    fn store_read_value(&mut self, record: &mut dyn Record, val: EpicsValue) -> bool {
        if self.iface_type == "asynFloat64" {
            if let EpicsValue::Double(raw) = val {
                if record.get_field("SMOO").is_some() {
                    let eng = self.apply_float64_ai_conversion(record, raw);
                    let _ = record.set_val(EpicsValue::Double(eng));
                    return true;
                }
                // asynFloat64 ao output readback: the record owns the forward
                // ASLO/AOFF scaling (`VAL = value*ASLO + AOFF`), the float64
                // twin of `apply_raw_readback`. ai is caught above (it carries
                // SMOO); ao has ASLO/AOFF but no SMOO so it lands here. C
                // `processAo` (devAsynFloat64.c:647-649). Returning `true`
                // skips the forward convert, matching INIT_DO_NOT_CONVERT.
                if record.apply_float64_readback(raw) {
                    return true;
                }
            }
        }
        if self.iface_type == "asynInt32" || self.iface_type == "asynUInt32Digital" {
            if let EpicsValue::Long(raw) = val {
                // Records own their `raw -> record-value` mapping: ao's
                // engineering inverse, mbbo/bo/mbboDirect's state-table / 0-1 /
                // bit map (outputs), and bi's 0/1 map + mbbi's state index +
                // mbbiDirect's shift/bits (inputs). `apply_raw_readback`
                // stores RVAL and computes VAL, and we return `true` (computed)
                // so the framework skips the record's forward convert, which
                // would recompute RVAL from the stale VAL and discard the
                // readback. C `processAo` (devAsynInt32.c:973-994),
                // `processMbbo`/`processBo`
                // (devAsynInt32.c:1310-1330,1202-1203 /
                // devAsynUInt32Digital.c:945-962,731-732),
                // `processMbboDirect` (devAsynUInt32Digital.c:1084-1090),
                // `processBi` (devAsynInt32.c / devAsynUInt32Digital.c:689),
                // `processMbbi` (devAsynInt32.c:1270 / devAsynUInt32Digital.c:903)
                // and `processMbbiDirect` (devAsynUInt32Digital.c:1031) all set
                // both fields from the callback and return without re-converting.
                // The hook is iface-agnostic (it uses the record's own
                // SHFT/state table), so the one dispatch covers both the
                // asynInt32 and the asynUInt32Digital readback. ai routes via
                // the ESLO branch below; longin declines (default `false`).
                if record.apply_raw_readback(self.dset_rval(record, raw)) {
                    return true;
                }
                // asynInt32 ai linear convert: raw -> RVAL, then the framework
                // runs the `raw -> eng` convert (C `processAi` return 0). Gated
                // on ESLO (the ai linearization field); a no-ESLO record that
                // did not claim the readback above falls through to set_val.
                if self.iface_type == "asynInt32" && record.get_field("ESLO").is_some() {
                    let _ = record.put_field("RVAL", EpicsValue::Long(raw));
                    return false;
                }
            }
        }
        // asynOctet: C device support hands the reply BYTES straight to the
        // record's own buffer — `readIt(pasynUser, pwf->bptr, pwf->nelm, ...)`
        // for waveform (devAsynOctet.c:1054), `psi->val` for stringin,
        // `plsi->val` for lsi — and never through `dbPutConvertRoutine`. So the
        // delivered shape follows the destination buffer, not a conversion: an
        // array-backed VAL takes the bytes as a `CharArray`, a DBF_STRING VAL
        // keeps the `String`. Only CHAR/UCHAR waveforms can reach here — C
        // `initCommon` sets `pact = 1` on any other FTVL at init
        // (devAsynOctet.c:312-316) — so there is no element-type question.
        // Delivering a `String` instead made the bytes arrive as a DBR_STRING
        // put, which C's `putStringChar` parses with `epicsParseInt8` and
        // refuses for any non-numeric reply.
        //
        // The `n` bytes kept (see `octet_read_cap`) land as C lands them: a
        // waveform's NORD is `n`; lsi/lso take `val[n] = 0` and `len = n + 1`;
        // a DBF_STRING VAL reads as a C string, to the first NUL.
        if self.iface_type == "asynOctet" {
            if let EpicsValue::String(s) = &val {
                use epics_base_rs::server::records::{lsi::LsiRecord, lso::LsoRecord};
                let bytes = s.as_bytes();
                if let Some(any) = record.as_any_mut() {
                    if let Some(lsi) = any.downcast_mut::<LsiRecord>() {
                        lsi.val.set_read(bytes);
                        return true;
                    }
                    if let Some(lso) = any.downcast_mut::<LsoRecord>() {
                        lso.val.set_read(bytes);
                        return true;
                    }
                }
                let delivered = if record.val().is_some_and(|v| v.is_array()) {
                    EpicsValue::CharArray(bytes.into())
                } else {
                    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
                    EpicsValue::String(epics_base_rs::types::PvString::from_bytes(&bytes[..end]))
                };
                let _ = record.set_val(delivered);
                return true;
            }
        }
        let _ = record.set_val(val);
        true
    }

    /// The ASLO/AOFF/SMOO arithmetic of C `devAsynFloat64::processAi`
    /// (devAsynFloat64.c:596-602). `ASLO`/`AOFF`/`SMOO` are read live so a
    /// runtime change takes effect on the next read. SMOO primes on the first
    /// read (C skips smoothing while `pr->udf`); `smoo_primed` is the
    /// adapter-side `!udf` for the float64 path (the adapter is the sole VAL
    /// writer there), and the `!cur.is_finite()` guard mirrors C's
    /// `!finite(pr->val)`.
    fn apply_float64_ai_conversion(&mut self, record: &mut dyn Record, raw: f64) -> f64 {
        let field = |name: &str| match record.get_field(name) {
            Some(EpicsValue::Double(v)) => v,
            _ => 0.0,
        };
        let aslo = field("ASLO");
        let aoff = field("AOFF");
        let smoo = field("SMOO");
        let mut val64 = raw;
        if aslo != 0.0 {
            val64 *= aslo;
        }
        val64 += aoff;
        let cur = match record.val() {
            Some(EpicsValue::Double(v)) => v,
            _ => f64::NAN,
        };
        let out = if smoo == 0.0 || !self.smoo_primed || !cur.is_finite() {
            val64
        } else {
            cur * smoo + val64 * (1.0 - smoo)
        };
        self.smoo_primed = true;
        out
    }

    /// The raw value an output record sends to the device — the write-side
    /// twin of [`AsynDeviceSupport::store_read_value`]'s readback. C device
    /// support writes the record's raw, post-OROC output, **not** the
    /// engineering `VAL`:
    /// - `asynInt32` / `asynUInt32Digital`: `pr->rval`, the raw the record's
    ///   convert produced from `OVAL` — `processAo` (devAsynInt32.c:997),
    ///   `processMbbo` (:1332), `processBo` (:1206), `processMbboDirect`
    ///   (devAsynUInt32Digital.c). `longout`/`int64out` carry no `RVAL` (VAL
    ///   *is* the raw) so they keep `VAL`, matching `processLongout`.
    /// - `asynFloat64` ao: `(OVAL - AOFF) / ASLO`, anchored on the
    ///   OROC-rate-limited `OVAL` (devAsynFloat64.c:651-654) — the inverse of
    ///   [`Record::apply_float64_readback`], so a value written, read back, and
    ///   re-scaled round-trips.
    ///
    /// `RVAL`/`OVAL` are current here: the OUT stage runs after the record's
    /// convert (the soft OUT-link write already anchors on `OVAL`). Default
    /// ASLO=1/AOFF=0 and an identity LINR keep `rval == val`, so the
    /// unconfigured output is unchanged. The previous `record.val()` anchor
    /// dropped the eng→raw conversion (int32 ao/mbbo) and OROC ramping
    /// (float64 ao) at the device.
    fn device_output_value(&self, record: &dyn Record) -> Option<EpicsValue> {
        match self.iface_type.as_str() {
            "asynFloat64" => {
                // Anchor on OVAL (post-OROC); fall back to VAL if absent.
                let oval = match record.get_field("OVAL") {
                    Some(EpicsValue::Double(o)) => Some(o),
                    _ => match record.val() {
                        Some(EpicsValue::Double(v)) => Some(v),
                        _ => None,
                    },
                };
                match oval {
                    Some(oval) => {
                        let field = |name: &str| match record.get_field(name) {
                            Some(EpicsValue::Double(x)) => x,
                            _ => 0.0,
                        };
                        let aslo = field("ASLO");
                        let aoff = field("AOFF");
                        let mut out = oval - aoff;
                        if aslo != 0.0 {
                            out /= aslo;
                        }
                        Some(EpicsValue::Double(out))
                    }
                    None => record.val(),
                }
            }
            "asynInt32" | "asynUInt32Digital" => match record.get_field("RVAL") {
                Some(EpicsValue::Long(r)) => Some(EpicsValue::Long(r)),
                Some(EpicsValue::ULong(r)) => Some(EpicsValue::Long(r as i32)),
                _ => record.val(),
            },
            _ => record.val(),
        }
    }

    /// Push a driver's asynEnum table onto the record's state fields at init
    /// — the C `setEnums` call from `initCommon:314`. Discriminates the
    /// record family via [`EnumRecordShape::of_record`] and applies the
    /// [`enum_table_fields`] delta directly to the record. Anything that is
    /// not an enum state record is left untouched. The runtime
    /// re-propagation path ([`Self::property_post_receiver`]) reuses the
    /// same [`enum_table_fields`] producer, posting the delta DBE_PROPERTY
    /// instead of writing the record in-band.
    fn apply_enum_table(&self, record: &mut dyn Record, entries: &[crate::param::EnumEntry]) {
        let Some(shape) = EnumRecordShape::of_record(record) else {
            return;
        };
        for (field, value) in enum_table_fields(shape, entries) {
            let _ = record.put_field(&field, value);
        }
    }

    /// Snapshot the Mode 1 averaging decimation threshold from the record's
    /// SVAL. C `interruptCallbackAverage` reads `pai->sval` live each callback
    /// (devAsynInt32.c:674); the callback runs off the record thread here, so
    /// the count is snapshotted into the shared atomic at init and on each
    /// process instead — a runtime SVAL change takes effect at the next
    /// process. `numToAverage = (int)(sval + 0.5)`, floored at 1
    /// (devAsynInt32.c:674-675). No-op for non-averaging records.
    fn refresh_average_decimation_threshold(&self, record: &dyn Record) {
        if let Some(acc) = &self.average {
            if let Some(EpicsValue::Double(sval)) = record.get_field("SVAL") {
                let n = (sval + 0.5) as i64;
                acc.num_to_average
                    .store(n.max(1), std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

impl DeviceSupport for AsynDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        self.dset = self.bound_dset(record.record_type());
        self.settle_ring();
        let ts_buf = match self.dset.check_ftvl(&self.record_name, record) {
            Ok(buf) => buf,
            Err(dead) => return Ok(dead),
        };
        // busy is the driver-clearable output: the busy module's devBusyAsyn
        // registers the output interrupt callback UNCONDITIONALLY (`if
        // (interruptCallback)` with no `asyn:READBACK` info gate,
        // devBusyAsyn.c:245-260) — that always-on readback is the record's
        // whole point (driver holds VAL=1, clears to 0 on completion). Forced
        // here, not info-gated: a C db's busy records carry no info tag.
        if record.record_type() == "busy" {
            self.set_asyn_readback(true);
        }
        // C `initCommon` (devAsynInt32.c:249-262): `pasynManager->connectDevice`
        // before `findInterface`/`drvUser->create`, and it is that call which
        // creates the device on a multi-device port (`locateDevice(..., TRUE)`,
        // asynManager.c:1349-1352) - so `asynReport` counts an address the
        // moment a record binds to it. C's failures (:255-259) are a port it
        // cannot find, and this handle is that port.
        self.handle.connect_device(self.addr);
        if self.handle.can_block() {
            self.request_cell();
        }
        // C calls drvUser->create only when the port registered asynDrvUser
        // **and** the record named a userParam (`if (pasynInterface &&
        // pPvt->userParam)`, devAsynInt32.c:264-265; the same pair of
        // conditions in devAsynFloat64.c, devAsynOctet.c, asynOctetSyncIO.c:141).
        // A byte transport registers no asynDrvUser and has no parameter to
        // name: the record binds at reason 0 — the port's own reason space —
        // and that is not a bind failure. Ask the interface registry
        // (`has_interface`), never a failed `create`.
        let drv_user = !self.reason_set
            && self.handle.has_interface(InterfaceType::DrvUser)
            && !self.drv_info.is_empty();
        if !self.reason_set {
            if drv_user {
                // Pass the record's asyn `addr`: a multi-device driver (e.g. modbus)
                // rejects an out-of-range offset here at bind time (C `drvUserCreate`
                // `checkOffset`) instead of alarming on every I/O. Pass the record's
                // interface (from its DTYP): an on-demand driver must create the
                // parameter with the type this record will read it as — C
                // `adsAsynPortDriver::getRecordInfoFromDrvInfo`.
                let req = DrvUserRequest::new(&self.drv_info, self.addr).with_iface(self.iface);
                match self.handle.drv_user_create_blocking(&req) {
                    Ok(info) => {
                        self.reason = info.reason;
                        // Per-record octet cap (C `modbusDrvUser_t.len`); applied to
                        // `octet_max_size` below, after SIZV finalizes the buffer.
                        self.octet_len_cap = info.max_octet_len;
                        self.drv_user = info.drv_user;
                    }
                    Err(e) => {
                        // Param not found, or the driver rejected the bind (e.g. an
                        // out-of-range offset) — this record cannot bind to a param.
                        // C prints and `goto bad` (devAsynInt32.c:272-276). Leaving
                        // the record live would let it keep processing against
                        // reason 0 — a different parameter.
                        self.reason_set = false;
                        self.dset.report(
                            &self.record_name,
                            &LinkRefusal::DrvUser(drv_user_message(&e)),
                        );
                        return Ok(self.dset.bad());
                    }
                }
            }
            self.reason_set = true;
        }
        if let Some(sync_io) = self.dset.sync_io() {
            if let Err(dead) = self.sync_io_connect(drv_user, sync_io) {
                return Ok(dead);
            }
        }

        // asynOctetCmdResponse misconfiguration: C initCmdBuffer
        // (devAsynOctet.c:631-637) rejects the command outright — `precord->pact
        // = 1`, recGblSetSevr(LINK_ALARM, INVALID_ALARM), INIT_ERROR — when, and
        // ONLY when, the RAW userParam (the pre-escape DRVINFO) is empty:
        // `strlen(pPvt->userParam) == 0`. The translated byte length (bufLen) is
        // computed later (:641) and may be 0 for a non-empty DRVINFO that escapes
        // to a leading NUL — that case is NOT rejected; C does a 0-byte
        // writeIt+readIt. So gate the reject on the raw DRVINFO, not on the
        // post-truncation command: a Some(empty) octet_cmd from a leading-NUL
        // command falls through to a 0-byte OctetWriteRead. octet_cmd is None for
        // a plain asynOctetRead, so this only fires for a CmdResponse record.
        if self.octet_cmd.is_some() && self.drv_info.is_empty() {
            epics_libcom_rs::epics_println!(
                "{}  devAsynOctet::initCmdBuffer no userParam",
                self.record_name
            );
            return Ok(DeviceInitOutcome::dead_with_alarm(
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid,
            ));
        }

        // C `initDbAddr` (devAsynOctet.c:645-657): the command record must
        // exist at init, or the record is dead at LINK/INVALID.
        if let Some(src) = self.device_job.write_read.get() {
            let held = Some((self.record_name.as_bytes(), &*record as &dyn Record));
            if !src.db.get().is_some_and(|db| db.has_pv(&src.pv, held)) {
                epics_libcom_rs::epics_println!(
                    "{} devAsynOctet::initDbAddr record {} not present",
                    self.record_name,
                    src.pv
                );
                return Ok(DeviceInitOutcome::dead_with_alarm(
                    epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                    epics_base_rs::server::record::AlarmSeverity::Invalid,
                ));
            }
        }

        // Read NELM from the record to set max_array_elements for array reads.
        // NELM is DBF_ULONG (waveform/aai/aao) and DBF_USHORT (histogram); take
        // the numeric view so no declaration is missed.
        if let Some(nelm) = record.get_field("NELM").and_then(|v| v.to_f64()) {
            if nelm > 0.0 {
                self.max_array_elements = nelm as usize;
            }
        }

        // The time-series buffer `check_ftvl` chose, sized to NELM.
        if let (Some(ts), Some(buf)) = (&self.time_series, ts_buf) {
            let mut st = ts.inner.lock().unwrap();
            st.nelm = self.max_array_elements;
            st.buf = Some(buf);
        }

        // C devAsynOctet `valSize`. SIZV is DBF_USHORT
        // (lsiRecord/lsoRecord/printfRecord.dbd.pod).
        self.octet_val_size = match (record.get_field("SIZV"), record.get_field("NELM")) {
            (Some(EpicsValue::UShort(sizv)), _) if sizv > 0 => sizv as usize,
            (_, Some(nelm)) => nelm.to_f64().map_or(0, |n| n as usize),
            _ => MAX_STRING_SIZE,
        };
        self.octet_max_size = self.octet_val_size;

        // Apply the per-record octet cap from `drv_user_create` after SIZV has
        // finalized the buffer length, mirroring C `getStringLen`: the effective
        // length is `min(buffer_len, drvUser->len)` (drvModbusAsyn.cpp:2367-2377).
        // A driver with no cap returns `None`, leaving the buffer length intact.
        if let Some(cap) = self.octet_len_cap {
            self.octet_max_size = self.octet_max_size.min(cap);
        }

        // asynUInt32Digital MASK/SHFT propagation. C devAsynUInt32Digital.c
        // init* (bi/bo :668/:710, mbbi/mbbo/mbbiDirect/mbboDirect :880,
        // :924, :1009, :1053) sets, whatever the mask, a 0 one too:
        //
        //     pr->mask = pPvt->mask;
        //     pr->shft = computeShift(pPvt->mask);   /* mbb records only */
        //
        // so the record's standard RVAL→VAL conversion shifts the
        // masked bits down to bit 0. Without this, a link like
        // `@asynMask(port,0,0xFF00) BITS` reads RVAL with bits 8-15
        // set but the record's shft stays 0 and VAL ends up
        // 65280-shaped instead of 0-255. bi/bo have no SHFT, so that put
        // is refused there.
        if self.iface_type == "asynUInt32Digital" {
            let shft = compute_mask_shift(self.mask);
            let _ = record.put_field("MASK", EpicsValue::Long(self.mask as i32));
            let _ = record.put_field("SHFT", EpicsValue::Short(shft as i16));
        }

        // asynInt32 mbbi/mbbo MASK positioning. C devAsynInt32.c initMbbi
        // (1246-1247) / initMbbo (1290-1291):
        //
        //     if (pr->nobt == 0) pr->mask = 0xffffffff;
        //     pr->mask <<= pr->shft;
        //
        // The asynInt32 device support POSITIONS the record's mask so
        // `rval = value & mask` (processMbbi/processMbbo) selects the field at
        // bits [SHFT, SHFT+NOBT). Without it, `apply_raw_readback`'s
        // `raw & mask` strips exactly the SHFT-selected bits (and a NOBT=0
        // record masks every bit to 0). uint32digital records get their
        // already-positioned `@asynMask` above instead; only mbbi/mbbo carry
        // NOBT under asynInt32 (bi/bo/ai/ao/longin have none; mbbiDirect/
        // mbboDirect are uint32digital-only), so NOBT presence selects them.
        //
        // C shifts the record's CURRENT mask, overriding to 0xffffffff only
        // when NOBT==0. mbbiRecord/mbboRecord init (mbbiRecord.c:129-130) sets
        // that current mask to a `.db`-loaded MASK, or to `(1<<NOBT)-1` only
        // when MASK was 0 — so mirroring `current << SHFT` here (rather than
        // rebuilding `(1<<NOBT)-1`) preserves a `.db`-set custom MASK and
        // yields 0 for the degenerate NOBT>32 (record mask stays 0), both
        // exactly C. `wire_device_to_record` (device_support.rs) calls `init`
        // exactly once per record, like C's once-only `initMbbi`, so the
        // in-place positioning is never re-applied (no double-shift).
        if self.iface_type == "asynInt32" {
            if let Some(EpicsValue::UShort(nobt)) = record.get_field("NOBT") {
                let shft = match record.get_field("SHFT") {
                    Some(EpicsValue::UShort(s)) => s as u32,
                    _ => 0,
                };
                let base: u32 = if nobt == 0 {
                    0xFFFF_FFFF
                } else {
                    match record.get_field("MASK") {
                        Some(EpicsValue::ULong(m)) => m,
                        _ => 0,
                    }
                };
                let positioned = base.checked_shl(shft).unwrap_or(0);
                let _ = record.put_field("MASK", EpicsValue::Long(positioned as i32));
            }
        }

        // C `initAiAverage` registers its callback after `initCommon` and before
        // `getBounds` (devAsynInt32.c:866-881).
        if self.average.is_some() {
            if let Err(e) = self.subscribe_average() {
                self.report_register_refusal(Register::Average, &e);
            }
        }

        // ai/ao LINEAR ESLO/EOFF wiring.
        //
        // C devAsynInt32.c::initAi (line 824-828) / initAo / initAiAverage:
        //
        //     if (deviceLow == 0 && deviceHigh == 0) {
        //         pasynInt32SyncIO->getBounds(..., &deviceLow, &deviceHigh);
        //     }
        //     convertAi(pr, 1);   // line 437-454: ESLO/EOFF from EGUF/EGUL+bounds
        //
        // The bounds query is only meaningful for asynInt32/asynInt64
        // (asynFloat64 has no getBounds in C; mbbi/mbbo use the mask
        // path computed above). The record only applies the result
        // when LINR != NO_CONVERSION, but C writes ESLO/EOFF
        // unconditionally and lets record processing decide whether
        // to honour it.
        if (self.iface_type == "asynInt32" || self.iface_type == "asynInt64")
            && record.get_field("ESLO").is_some()
        {
            self.init_device_bounds();
            self.apply_linear_eslo_eoff(record);
        }

        // Driver enum-string table -> record state fields. C asyn int32/uint32
        // device support (devAsynInt32.c::initCommon:298-324,
        // devAsynUInt32Digital.c:255-280) enters this block on
        // `findInterface(asynEnumType) && maxEnums>0`, after an EnumSyncIO
        // connect (devAsynInt32.c:300-311); the record family is identified by
        // the state fields it exposes (ZRST or ZNAM). Inside it only `setEnums`
        // waits on a successful read: `callbackEnum` registers whether or not
        // the driver returned a table.
        let enum_table = EnumRecordShape::of_record(record).filter(|_| {
            self.dset.connects_enum_sync_io() && self.handle.has_interface(InterfaceType::Enum)
        });
        if let Some(shape) = enum_table {
            if let Err(dead) = self.sync_io_connect(drv_user, "EnumSyncIO") {
                return Ok(dead);
            }
            self.enum_shape = Some(shape);
            let user = self.init_user();
            if let Ok(result) = self.handle.submit_blocking(RequestOp::EnumRead, user) {
                if let Some(entries) = result.enum_entries {
                    // The table setEnums leaves in the record, which is also
                    // what a ring slot carries back, and the runtime
                    // callback's diff seed.
                    let entries = clamp_enum_table(&entries);
                    self.apply_enum_table(record, &entries);
                    self.enum_choices = Some(entries);
                }
            }
            match self.subscribe_enum() {
                Ok(rx) => self.enum_intr_rx = Some(rx),
                Err(e) => self.report_register_refusal(Register::Enum, &e),
            }
        }

        // C `initCommon` registers the `asyn:READBACK` callback here, after the
        // enum table and ahead of the initial readback (devAsynInt32.c:325-341,
        // and the same step in each devAsyn file).
        if self.asyn_readback && self.average.is_none() && self.time_series.is_none() {
            if let Err(e) = self.subscribe_input() {
                self.report_register_refusal(Register::ReadBack, &e);
            }
        }

        let mut read_back = false;
        if self.initial_readback {
            // devAsynOctet connects its SyncIO only for this read
            // (devAsynOctet.c:361); the other dsets connected above.
            if self.dset == Dset::Octet {
                if let Err(dead) = self.sync_io_connect(drv_user, "octetSyncIO") {
                    return Ok(dead);
                }
            }
            if let Some(op) = self.read_op(&*record) {
                let user = self.init_user();
                if let Ok(mut result) = self.handle.submit_blocking(op, user) {
                    Int32Mask::apply_to_read(self.int32_mask, &mut result);
                    // Seed the output record's value only on a successful read,
                    // mirroring C initAo/initLongout/initMbbo: `if (status ==
                    // asynSuccess) { pao->rval = value; return INIT_OK; }` with
                    // `INIT_DO_NOT_CONVERT` the FAILURE fall-through
                    // (initAo devAsynInt32.c:955-959, initMbbo :1295-1299;
                    // initLo, which has no RVAL to convert, stores VAL under the
                    // same guard and returns INIT_OK either way, :1082-1086).
                    // A failed initial read, a parameter's status included, is
                    // an `Err` and leaves the .db default.
                    if let Some(val) = self.result_to_value(&result) {
                        // ao seeds VAL from the raw readback via its
                        // `raw -> eng` inverse: C `initAo` sets `rval=value`
                        // and returns INIT_OK so aoRecord runs the readback
                        // convert (devAsynInt32.c:947-957). longout/mbbo
                        // have no such inverse (VAL == raw / state-map) and
                        // decline the hook (default `false`). An asynFloat64
                        // ao seeds VAL with the forward ASLO/AOFF scaling
                        // (C `initAo`, devAsynFloat64.c:628-630) via the
                        // float64 readback hook.
                        let seeded = match val {
                            EpicsValue::Long(raw) => {
                                let rval = self.dset_rval(&*record, raw);
                                record.apply_raw_readback(rval)
                            }
                            EpicsValue::Double(raw) => record.apply_float64_readback(raw),
                            _ => false,
                        };
                        // The seed lands as every read does, through
                        // `store_read_value`: a DBF_STRING VAL is C's
                        // `strcpy` of the reply, cut at the first NUL
                        // (devAsynOctet.c:373-376).
                        if !seeded {
                            self.store_read_value(record, val);
                        }
                        read_back = true;
                    }
                }
            }
        }

        // Mode 1 averaging: snapshot the SVAL decimation count so the first
        // decimation period uses the configured value before the first process
        // refreshes it (C reads pai->sval live; devAsynInt32.c:674).
        if self.is_io_intr_scan() {
            self.refresh_average_decimation_threshold(record);
        }
        Ok(init_status(
            record.record_type(),
            &self.iface_type,
            read_back,
        ))
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        if !self.reason_set {
            return Ok(DeviceReadOutcome::ok());
        }

        // Time-series device support: the record process. C
        // `devAsynXXXTimeSeries.h::process` applies RARM, commits NORD/BUSY, and
        // resets RARM — the accumulating callback owns the array. RARM is a
        // process-passive field (waveformRecord.dbd.pod RARM pp(TRUE)), so a
        // `caput RARM` lands here; the buffer-full callback and the Read FLNK
        // also drive process. The buffer is committed to VAL (which sets NORD)
        // rather than the callback writing the record array directly — the
        // asyn-rs sync callback holds no record lock.
        if let Some(ts) = self.time_series.clone() {
            use epics_base_rs::server::records::waveform::WaveformRecord;
            let Some(wf) = record
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<WaveformRecord>())
            else {
                // TimeSeries dsets are waveform-only; a non-waveform record
                // cannot carry RARM/BUSY — nothing to commit.
                return Ok(DeviceReadOutcome::ok());
            };
            // `buf` is Some here by construction: init returns
            // `DeviceInitOutcome::Dead` on an invalid FTVL, so a record that
            // reached process has a typed buffer.
            let rarm = wf.rarm;
            let (arr, busy, status) = {
                let mut st = ts.inner.lock().unwrap();
                // C process RARM switch (devAsynXXXTimeSeries.h): 1=start
                // (zero nord + arm), 2=stop, 3=resume, 0=no-op. "Start" clears
                // the buffer; the others only flip BUSY.
                match rarm {
                    1 => {
                        if let Some(buf) = st.buf.as_mut() {
                            buf.clear();
                        }
                        // C `memset(pwf->bptr, 0, pwf->nelm*sizeof(EPICS_TYPE))`
                        // (devAsynXXXTimeSeries.h:139-141): a VAL put leaves
                        // the elements past its own length alone.
                        wf.reallocate_val();
                        st.busy = true;
                    }
                    2 => st.busy = false,
                    3 => st.busy = true,
                    _ => {}
                }
                let arr = st.buf.as_ref().map(TsBuf::to_epics_value);
                let busy = st.busy;
                // Consume the accumulated transport status (C applies it then
                // resets pPvt->status = asynSuccess).
                let status = std::mem::replace(&mut st.status, crate::error::AsynStatus::Success);
                (arr, busy, status)
            };
            // Commit the current samples to VAL — `set_val` routes to
            // put_field("VAL") for waveform and sets NORD = len (C
            // `pwf->nord = pPvt->nord`). A None buffer (invalid FTVL) commits
            // nothing, leaving VAL/NORD untouched.
            if let Some(arr) = arr {
                let _ = wf.set_val(arr);
            }
            // Sync the device-only fields the framework does not touch: BUSY
            // reflects acquisition; RARM resets to 0 (C `pwf->rarm = 0`).
            // C `process` registers the callback on BUSY 0->1 and cancels it
            // on 1->0 (devAsynXXXTimeSeries.h:155-176); the callback clearing
            // `st.busy` at buffer-full leaves it registered until then.
            if (wf.busy != 0) != busy {
                self.time_series_sub = if busy {
                    self.time_series_subscribe(ts.clone())
                } else {
                    None
                };
            }
            wf.busy = busy.into();
            wf.rarm = 0;
            // C `if (pPvt->status != asynSuccess) recGblSetSevr(prec, READ_ALARM,
            // INVALID_ALARM)`; success clears to NO_ALARM. Input default
            // READ_ALARM (waveform is an input record).
            let (a_status, a_sev) = resolve_intr_alarm(
                (0, 0),
                status,
                epics_base_rs::server::recgbl::alarm_status::READ_ALARM,
            );
            self.last_alarm_status = a_status;
            self.last_alarm_severity = a_sev;
            // VAL written directly → skip the record's RVAL→VAL convert.
            // C `devAsynXXXTimeSeries.h::processCommon` commits the array and
            // the record is defined by it; `Defined` is that fact.
            return Ok(DeviceReadOutcome::computed(DeviceUdf::Defined));
        }

        // Write-only (asynOctetWrite / asynOctetWriteBinary): waveform is an input
        // record type so process calls read(), not write(). Perform the write
        // here instead. The binary variant sends the full NORD bytes (no NUL-trim);
        // the text variant trims at the first NUL via write_op.
        //
        // Route through `device_output_value` like the write()/write_begin()
        // entries so the raw-output anchor invariant holds by construction, not
        // by "this path is octet-only": for asynOctet it returns VAL unchanged
        // (the `_` arm), but a future scalar write_only DTYP would then anchor on
        // RVAL/OVAL automatically instead of silently bypassing it.
        if self.write_only {
            // A blocking port ran the write queued by `read_begin`; any other
            // port writes now.
            let ran = match self.take_ran() {
                Some(ran) => {
                    self.report_queued(ran.as_ref().err());
                    Some(ran)
                }
                None => self
                    .write_only_op(&*record)
                    .map(|op| self.submit_traced(op)),
            };
            if let Some(ran) = ran {
                if !matches!(&ran, Err(e) if self.drop_refusal(e)) {
                    // C callbackWfWrite/WriteBinary: writeIt, then processCommon
                    // raises the result's alarm (devAsynOctet.c:1071-1091).
                    (self.last_alarm_status, self.last_alarm_severity) =
                        self.output_alarm(write_alarm(&ran));
                }
            }
            return Ok(DeviceReadOutcome::ok());
        }

        // Mode 1 averaging (SCAN="I/O Intr"): the decimated mean is delivered
        // through the IoIntr ring — the average callback decimates every
        // round(SVAL) samples into `interrupt_fifo` and wakes the record (C
        // `interruptCallbackAverage` isIOIntrScan branch, devAsynInt32.c:
        // 673-702). Refresh the live SVAL threshold; the ring-pop path below
        // drains the decimated value (C `processAiAverage` `getCallbackValue`
        // branch, :895-898).
        if self.average.is_some() && self.is_io_intr_scan() {
            self.refresh_average_decimation_threshold(record);
        }
        // A blocking port ran the read queued by `read_begin`, or the queue
        // refused it there.
        let ran = self.take_ran();
        // Pop the oldest entry from the ring buffer in any SCAN. C parity:
        // `devAsynInt32.c::getCallbackValue` — returns the next FIFO entry,
        // logs+resets the overflow counter on consume; `processAi` (:836) and
        // `processAiAverage` (:895) call it ungated, so an entry queued before
        // a move off I/O Intr is still read. An `asyn:READBACK` output record consumes
        // from the same ring: the framework calls `read()` on it only on a
        // driver-callback cycle (`process_record_readback`), so popping
        // the FIFO here is the output-record analogue of
        // `processBo`'s `getCallbackValue` readback branch.
        {
            // The record reached its read stage, so this driver-callback
            // cycle processed: clear the armed flag (C `processBo` clears
            // `newOutputCallbackValue` when it runs). A survived flag after
            // the cycle means the PACT guard bailed before here, which
            // `reconcile_readback_callback` then repairs.
            self.output_callback_pending = false;
            // C `processAi` pops the ring before it queues and again on the
            // pass the queued read re-enters, where a value that arrived since
            // replaces the read's result. A refused queue is this same pass,
            // whose ring C already found empty.
            let refused = matches!(&ran, Some(Err(e)) if e.is_queue_refused());
            let (entry, ring) = if refused {
                (None, false)
            } else {
                let mut fifo = self.interrupt_fifo.lock().unwrap();
                (fifo.pop(), !fifo.is_latest())
            };
            if let Some(d) = &entry {
                self.popped(d);
            }
            if let Some(Delivery {
                entry: ci,
                store,
                spent,
                ..
            }) = entry
            {
                // C's negative-status default: nothing has been stored yet, so
                // unless a store below lands a value this cycle sourced none
                // (`return -1`) — a transport error, or a value this record's
                // interface cannot read. Starting here rather than at
                // `computed()` is what keeps a store-nothing path from
                // declaring the record defined.
                let mut read_outcome = DeviceReadOutcome::failed();
                // Direction-aware default for the transport-status mapping: C
                // `asynStatusToEpicsAlarm` takes READ_ALARM for input device
                // support (processAi, devAsynInt32.c:844) and WRITE_ALARM for
                // scalar output readback (processBo/Lo/Mbbo, :1219). Array dsets
                // use READ_ALARM even for aao output (devAsynXXXArray.cpp's
                // single shared process(), :330-331), so an array iface is
                // always READ_ALARM; otherwise an asyn:READBACK output is
                // WRITE_ALARM and a plain input I/O Intr is READ_ALARM. The
                // default only affects the asynError/unknown arm.
                let default_stat = if self.iface_type.ends_with("Array") {
                    epics_base_rs::server::recgbl::alarm_status::READ_ALARM
                } else if self.asyn_readback {
                    epics_base_rs::server::recgbl::alarm_status::WRITE_ALARM
                } else {
                    epics_base_rs::server::recgbl::alarm_status::READ_ALARM
                };
                // Apply the sample's alarm with the transport-status fill-in
                // (C devAsynInt32.c:844-847; sample EPICS alarm wins, transport
                // status fills only NO_ALARM fields). Unconditional, like the
                // pop/overflow/output_callback_pending/timestamp above and here
                // — C sets `time` even on a transport error
                // (devAsynXXXArray.cpp:327) and always recGblSetSevr's the mapped alarm.
                let (status, severity) = resolve_intr_alarm(
                    (ci.alarm_status, ci.alarm_severity),
                    ci.aux_status,
                    default_stat,
                );
                self.last_alarm_status = status;
                self.last_alarm_severity = severity;
                self.last_ts = Some(ci.timestamp.into());
                // C gates the value store on the transport status:
                // `if (result.status == asynSuccess) { pr->rval = … } else
                // return -1` (processAi devAsynInt32.c:844-855; processBo
                // stores at :1201-1204 and returns -1 at :1229-1234), and the
                // array process() copies bptr/nord only
                // `if (rp->status == asynSuccess)` (devAsynXXXArray.cpp:317).
                // On a transport error keep the prior value (skip the store);
                // `read_outcome` stays `failed()` so the record skips the
                // RVAL→VAL convert, keeps its previous value and re-derives no
                // UDF = C return -1. The latest slot's good value is stored
                // under a failed status too: C copied it into the record at
                // callback time (devAsynOctet.c:492-501). A spent entry's
                // status was reset to success: `udf = 0; return 0`, under the
                // same alarm. Its LEN/NORD come from the value stored: C takes
                // them from the last callback's length, a failed one's too
                // (:505, :776), so its reset pass (:817-820) can post a LEN
                // that no longer describes VAL, a C defect not copied.
                let good = ci.aux_status == crate::error::AsynStatus::Success || spent;
                if store {
                    // Derive the value through THIS record's interface, the same
                    // way the polled path does (`result_to_value`): an array
                    // interrupt gets the per-interface `convert` (C fires all six
                    // array interfaces, NDPluginStdArrays.cpp:170-197) and a
                    // scalar gets its interface's read rule.
                    match self.param_value_for_iface(&ci.value, ci.octet_nul) {
                        Ok(Some(val)) => {
                            let val = self.cap_octet_read_value(val);
                            let wrote_val = self.store_read_value(record, val);
                            if good {
                                read_outcome = stored(wrote_val);
                            }
                            // C's ring-0 array copy is the callback's, and
                            // dumps nothing.
                            if self.dset == Dset::Array && ring {
                                self.print_callback_value(record);
                            }
                        }
                        Ok(None) => {}
                        // The driver fired a value this record's interface cannot
                        // read. C cannot deliver that (one interrupt list per
                        // interface); here it means the parameter's type and the
                        // record's DTYP disagree. Alarm it — the same
                        // TypeMismatch the polled read raises through
                        // `asyn_error_to_alarm` — and keep the prior value.
                        // Coercing it instead would bypass the record's
                        // conversions silently.
                        Err(e) => {
                            let (status, severity) =
                                asyn_error_to_alarm_with_default(&e, default_stat);
                            self.last_alarm_status = status;
                            self.last_alarm_severity = severity;
                        }
                    }
                } else if spent {
                    read_outcome = stored(false);
                }
                if self.dset == Dset::Octet {
                    self.print_callback_value(record);
                }
                // Whatever the store decided, or the `failed()` this started
                // as when no store ran at all.
                return Ok(read_outcome);
            }
            // A readback cycle runs only on a driver callback, so it sources
            // nothing here. An input with an empty ring — PINI before the first
            // callback — reads as a polled record does: C `if
            // (!getCallbackValue(pPvt) && !pr->pact) queueRequest`.
            if self.asyn_readback {
                return Ok(DeviceReadOutcome::failed());
            }
        }

        // Mode 2 averaging (periodic SCAN), and Mode 1 with an empty ring: the
        // always-on interrupt callback (`subscribe_average`) has been
        // accumulating samples into the SumAverager since the gate opened. Drain the arithmetic mean
        // since the last process and reset — C `processAiAverage` else branch
        // (devAsynInt32.c:887-928, devAsynFloat64.c:708-755). Reuses the normal
        // ai store paths: int32 routes the rounded mean to RVAL (the ai record's
        // convert applies ESLO/EOFF), float64 sets VAL directly (ASLO/AOFF/SMOO
        // applied in-dset) — averaging only changes the *source* of the raw
        // value, exactly as CmdResponse reused the octet reply→VAL.
        if let Some(acc) = self.average.clone() {
            // C `processAiAverage` computes and RESETS the accumulator
            // (numAverage=0, sum=0) BEFORE the transport-status check, so the
            // period's samples are consumed even on a transport-error cycle —
            // the checked drain already resets the averager.
            let drained = if self.iface_type == "asynInt32" {
                acc.averager
                    .read_and_reset_int32_checked()
                    .map(EpicsValue::Long)
            } else {
                acc.averager
                    .read_and_reset_checked()
                    .map(EpicsValue::Double)
            };
            // Snapshot the two status channels atomically against the callback.
            // Reset the accumulated transport status only when a period was
            // consumed (C resets `result.status = asynSuccess` on the
            // failure branch; on the success branch it was already success).
            // A zero-samples cycle leaves it untouched — and count==0 means no
            // sample was pushed, so `aux_error` is `None` there regardless.
            let (last_alarm, transport) = {
                let mut st = acc.status.lock().unwrap();
                let snap = (st.last_alarm, st.aux_error);
                if drained.is_some() {
                    st.aux_error = None;
                }
                snap
            };
            match drained {
                Some(val) => {
                    // Final alarm = last sample's EPICS alarm, with C's
                    // `asynStatusToEpicsAlarm` fill-in (shared resolver). Average
                    // dsets are ai-only, so the input default READ_ALARM applies.
                    let (status, severity) = resolve_intr_alarm(
                        last_alarm,
                        transport.unwrap_or(crate::error::AsynStatus::Success),
                        epics_base_rs::server::recgbl::alarm_status::READ_ALARM,
                    );
                    self.last_alarm_status = status;
                    self.last_alarm_severity = severity;
                    if transport.is_some() {
                        // Transport-error period: C `processAiAverage` discards
                        // the averaged value — `if (result.status ==
                        // asynSuccess) { store } else { result.status =
                        // asynSuccess; return -1 }` (devAsynInt32.c:919-927,
                        // devAsynFloat64.c:736-754). `failed()` is that -1:
                        // no store, so RVAL/VAL keep their previous value, and
                        // no UDF re-derive either — C reaches the same place by
                        // missing `aiRecord.c:161`'s `status == 0` gate. The
                        // mapped READ/TIMEOUT/...@INVALID alarm above stands.
                        return Ok(DeviceReadOutcome::failed());
                    }
                    // Transport success: store the mean and define the
                    // record — C `processAiAverage` ends `pr->rval = rval;
                    // pr->udf = 0; return 0` (devAsynInt32.c:920-922).
                    let wrote_val = self.store_read_value(record, val);
                    return Ok(stored(wrote_val));
                }
                None => {
                    // No samples since the last process. C `processAiAverage`
                    // sets UDF_ALARM/INVALID, sets the record's `udf = 1` and
                    // returns -2 with VAL untouched — devAsynInt32.c:900-904,
                    // devAsynFloat64.c:721-725, devAsynInt64.c:829-833.
                    // `undefined()` is that -2: it skips the RVAL→VAL convert
                    // so VAL keeps its previous value, suppresses the framework's
                    // per-cycle UDF re-derive (which `aiRecord.c:161` gates on
                    // `status == 0`), and carries the `udf = 1` assertion for the
                    // framework to apply — device support holds a `&mut dyn
                    // Record` and cannot reach `RecordInstance.common` itself.
                    self.last_alarm_status = epics_base_rs::server::recgbl::alarm_status::UDF_ALARM;
                    self.last_alarm_severity =
                        epics_base_rs::server::record::AlarmSeverity::Invalid as u16;
                    return Ok(DeviceReadOutcome::undefined());
                }
            }
        }

        // Same negative-status default as the I/O Intr branch: a non-success
        // `processAi` read stores nothing and returns -1
        // (devAsynInt32.c:848-855), so the record neither converts nor
        // re-derives UDF.
        let mut read_outcome = DeviceReadOutcome::failed();
        // Any other port reads now.
        let ran = match ran {
            Some(ran) => {
                self.report_queued(ran.as_ref().err());
                Some(ran)
            }
            None => self
                .read_op(&*record)
                .map(|op| self.submit_read(op, &*record)),
        };
        if let Some(ran) = ran {
            match ran {
                Ok(mut result) => {
                    let failed = result.octet.take_if(|r| r.status().is_err());
                    if let Some((data, Err(e))) = failed.map(OctetRead::into_parts) {
                        // A failed octet read that still transferred bytes hands
                        // them to the record. C `devAsynOctet::readIt`
                        // (devAsynOctet.c:693-717) passes the record's own value
                        // buffer straight to `pasynOctet->read`, so on an
                        // `asynTimeout` after a partial line the driver has
                        // *already* written those bytes into `psi->val` /
                        // `pwf->bptr`; readIt returns the failing status but the
                        // data is in the record. Only the success-gated fields
                        // (UDF, NORD/LEN — callbackSiRead:924-931,
                        // callbackWfRead:1055-1061) stay untouched, which is why
                        // the store below pairs with `DeviceUdf::Untouched`.
                        if !data.is_empty() {
                            let val = self.octet_value(&data);
                            let wrote_val = self.store_read_value(record, val);
                            read_outcome = if wrote_val {
                                DeviceReadOutcome::computed(DeviceUdf::Untouched)
                            } else {
                                DeviceReadOutcome::ok()
                            };
                        }
                        // A parameter's status failed it in `readOctet`, which
                        // set the parameter's alarm and time (asynPortDriver.cpp:
                        // 2617-2619); a driver's failure carries neither.
                        let (alarm_status, alarm_severity) = resolve_intr_alarm(
                            (result.alarm_status, result.alarm_severity),
                            e.status(),
                            epics_base_rs::server::recgbl::alarm_status::READ_ALARM,
                        );
                        self.last_alarm_status = alarm_status;
                        self.last_alarm_severity = alarm_severity;
                        if let Some(ts) = result.timestamp {
                            self.last_ts = Some(ts.into());
                        }
                    } else {
                        if let Some(val) = self.result_to_value(&result) {
                            let wrote_val = self.store_read_value(record, val);
                            read_outcome = stored(wrote_val);
                        } else if self.iface_type == "asynOctet" {
                            // A write-read whose write came up short read
                            // nothing, and processCommon still succeeds
                            // (devAsynOctet.c:853-860, :816-818).
                            read_outcome = stored(true);
                        }
                        self.last_alarm_status = result.alarm_status;
                        self.last_alarm_severity = result.alarm_severity;
                        self.last_ts = result.timestamp.map(Into::into);
                    }
                }
                // A read that failed with its value, a parameter's status: C
                // processAi maps the alarm and sets the time before its status
                // gate and stores nothing (devAsynInt32.c:844-855).
                Err(AsynError::FailedRead { read, .. }) => {
                    self.last_alarm_status = read.alarm_status;
                    self.last_alarm_severity = read.alarm_severity;
                    self.last_ts = read.timestamp.map(Into::into);
                }
                // `processCommon` succeeds, defining the record (devAsynOctet.c:816-818,
                // devAsynXXXArray.cpp:334-336).
                Err(e) if self.drop_refusal(&e) => read_outcome = stored(true),
                Err(e) => {
                    // Convert asyn error to EPICS alarm (C parity: asynStatusToEpicsAlarm)
                    let (alarm_status, alarm_severity) = asyn_error_to_alarm(&e);
                    self.last_alarm_status = alarm_status;
                    self.last_alarm_severity = alarm_severity;
                }
            }
        }
        // Whatever the store decided, or the `failed()` this started as.
        Ok(read_outcome)
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            return Ok(());
        }
        let written = match self.device_output_value(record) {
            Some(val) => match self.write_op(&val) {
                Some(op) => self.write_now(op),
                None => Ok(()),
            },
            None => Ok(()),
        };
        self.end_processing();
        written
    }

    /// devAsynInt32.c:157-161 and devAsynInt64.c:153-157: the ai, aiAverage
    /// and ao dsets fill `special_linconv` with `convertAi`/`convertAo`. Only
    /// ai/ao have the LINR/EGUF/EGUL puts that call it.
    fn has_special_linconv(&self) -> bool {
        matches!(self.dset, Dset::Int32 { .. } | Dset::Int64)
    }

    fn special_linconv(&mut self, record: &mut dyn Record, after: bool) -> CaResult<()> {
        if after {
            self.apply_linear_eslo_eoff(record);
        }
        Ok(())
    }

    fn dtyp(&self) -> &str {
        &self.iface_type
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        if self.last_alarm_status == 0 && self.last_alarm_severity == 0 {
            None
        } else {
            Some(DeviceAlarm {
                stat: self.last_alarm_status,
                sevr: self.last_alarm_severity,
                msg: "",
            })
        }
    }

    fn last_timestamp(&self) -> Option<epics_base_rs::types::WallTime> {
        self.last_ts
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        self.record_name = name.to_string();
        self.io_intr_scan.store(
            scan == ScanType::IoIntr,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn apply_record_info(&mut self, info: &std::collections::HashMap<String, String>) {
        // C parity: `asynDbGetInfo(pr, "asyn:READBACK")` +
        // `asynDbGetInfo(pr, "asyn:INITIAL_READBACK")` at
        // devAsynInt32.c:329, devAsynFloat64.c:218,
        // devAsynInt64.c:257, devAsynUInt32Digital.c:286,
        // devAsynOctet.c:337+357, devAsynXXXArray.cpp:172.
        // C semantics: a non-NULL info-string is fed through `atoi`,
        // any non-zero numeric result enables the flag — which means
        // "0", "0x0", "00" disable; "1", "Y" (atoi → 0!), garbage
        // strings (atoi → 0) disable. We use the broader EPICS-style
        // "truthy / falsey" parse here so values like "Y" or "true"
        // also work (they would NOT work under strict atoi parity,
        // but the broader parse is a strict superset for "1" / "0").
        if let Some(raw) = info.get("asyn:READBACK") {
            self.set_asyn_readback(parse_info_bool(raw));
        }
        // The scalar outputs read at init whatever it says, and no input or
        // array dset reads at all.
        if let Some(raw) = info
            .get("asyn:INITIAL_READBACK")
            .filter(|_| self.output && self.dset == Dset::Octet)
        {
            self.set_initial_readback(parse_info_bool(raw));
        }
        // `ringSize = atoi(sizeString)` (devAsynInt32.c:361-362): text that
        // is no number is 0. A negative size crashes C (a NULL or negative
        // ring); the default stands.
        if let Some(raw) = info.get("asyn:FIFO") {
            if let Ok(n) = usize::try_from(epics_base_rs::types::c_parse::atoi(raw)) {
                self.fifo_tag = Some(n);
            }
        }
    }

    fn write_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        // Same raw-output anchor as the synchronous write() — the async path
        // (blocking ports, e.g. motors) must not bypass the RVAL/OVAL anchor.
        let val = match self.device_output_value(record) {
            Some(v) => v,
            None => return Ok(WriteStart::Synchronous),
        };
        let op = match self.write_op(&val) {
            Some(op) => op,
            None => return Ok(WriteStart::Synchronous),
        };

        // A port that cannot block (C `ASYN_CANBLOCK` clear) writes inside
        // this dbProcess call, as C's `processCallbackOutput` runs from the
        // record's own `process` on such a port, so CP chain targets see the
        // value at once. It is the whole write: `Completed`, never
        // `Synchronous`, or the framework would run `write()` and the driver
        // would see the value twice (a USB-CTR pulse generator restarted
        // twice per put).
        if !self.handle.can_block() {
            let written = self.write_now(op);
            self.end_processing();
            written?;
            return Ok(WriteStart::Completed);
        }

        self.arm_reentry(done);
        let user = self.process_user();
        if self.defers_output_callbacks() {
            self.interrupt_fifo.lock().unwrap().processing_async = true;
        }
        if let Err(e) = self.queue(op, user, Finish::Complete) {
            self.end_processing();
            if self.drop_refusal(&e) {
                return Ok(WriteStart::Completed);
            }
            // `processAo` raises the refusal's own status (devAsynInt32.c:1008).
            (self.last_alarm_status, self.last_alarm_severity) = self.output_alarm(Err(e.status()));
            return Err(asyn_to_ca_error(e));
        }
        Ok(WriteStart::Pending)
    }

    fn bind_database(&mut self, db: epics_base_rs::server::database::AsyncDbHandle) {
        if let Some(src) = self.device_job.write_read.get() {
            let _ = src.db.set(db);
        }
    }

    fn async_write_completed_common(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
    ) {
        let wrote = self.device_job.write_alarm.lock().unwrap_or(Ok((0, 0)));
        self.define_on_write(common, wrote.is_ok());
        (self.last_alarm_status, self.last_alarm_severity) = self.output_alarm(wrote);
        self.end_processing();
    }

    fn write_common(&mut self, record: &mut dyn Record, common: &mut CommonFields) -> CaResult<()> {
        let written = self.write(record);
        self.define_on_write(common, written.is_ok());
        written
    }

    fn write_begin_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        let start = self.write_begin(record, done);
        if let Ok(WriteStart::Completed) = start {
            self.define_on_write(common, true);
        }
        start
    }

    /// C `processAi` and its kin on an `ASYN_CANBLOCK` port: set PACT, queue
    /// the read and return (devAsynInt32.c:836-841); the port thread reads and
    /// re-processes the record, whose second pass reaches [`Self::read`]. A
    /// value the driver already delivered — an I/O Intr ring entry, readback,
    /// averaging, time series — is not queued, as C's `getCallbackValue`
    /// branch is not; an I/O Intr record with an empty ring is.
    fn read_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        if done.pact()
            || !self.reason_set
            || !self.handle.can_block()
            || self.time_series.is_some()
            || (!self.write_only && (self.average.is_some() || self.asyn_readback))
            || self.callback_value_waiting()
        {
            return Ok(ReadStart::Synchronous);
        }
        let op = if self.write_only {
            self.write_only_op(&*record)
        } else {
            self.read_op(&*record)
        };
        let Some(op) = op else {
            return Ok(ReadStart::Synchronous);
        };
        self.arm_reentry(done);
        let user = self.process_user();
        match self.queue(op, user, Finish::Reprocess) {
            Ok(()) => Ok(ReadStart::Pending),
            // C `reportQueueRequestStatus`: this pass applies the refusal.
            Err(e) => {
                *self.device_job.read.lock() = Some(Err(e));
                Ok(ReadStart::Synchronous)
            }
        }
    }

    /// The record's own database: the ring drops while it is closed. The
    /// port's flush and its seed follow the port's owner, not this gate.
    fn bind_interrupt_gate(
        &mut self,
        gate: &Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>,
    ) {
        let _ = self.gate.bound.set(gate.clone());
    }

    fn get_ioint_info(
        &mut self,
        record: &mut dyn Record,
        _common: &mut epics_base_rs::server::record::CommonFields,
        cmd: epics_base_rs::server::device_support::IoIntrCmd,
    ) -> bool {
        use epics_base_rs::server::device_support::IoIntrCmd;
        use std::sync::atomic::Ordering;
        // "If initCommon failed then pPvt->pint32 is NULL, return error".
        if !self.reason_set {
            return false;
        }
        // The time-series dsets have no get_ioint_info; BUSY drives theirs.
        if self.time_series.is_some() {
            return true;
        }
        let driver = self.dset.driver();
        let rec = &self.record_name;
        match cmd {
            IoIntrCmd::Add => {
                self.dev_print(
                    crate::trace::TraceMask::FLOW,
                    file!(),
                    line!(),
                    format_args!("{rec} {driver}::getIoIntInfo registering interrupt\n"),
                );
                self.io_intr_scan.store(true, Ordering::Relaxed);
                // An average's callback is registered in every SCAN; only the
                // decimation count it now reads matters (devAsynInt32.c:385).
                if self.average.is_some() {
                    self.refresh_average_decimation_threshold(record);
                } else if self.interrupt_sub.is_none() {
                    // An `asyn:READBACK` output already holds its `initCommon`
                    // client. C registers a second one on the same ring
                    // (devAsynInt32.c:386-389), overwriting `registrarPvt`, so
                    // each driver update lands twice and scans the record
                    // twice: a C defect, not copied.
                    if let Err(e) = self.subscribe_input() {
                        self.report_register_refusal(Register::IoIntr, &e);
                    }
                }
            }
            IoIntrCmd::Delete => {
                let canceling = if self.dset == Dset::Int64 {
                    "canceling"
                } else {
                    "cancelling"
                };
                self.dev_print(
                    crate::trace::TraceMask::FLOW,
                    file!(),
                    line!(),
                    format_args!("{rec} {driver}::getIoIntInfo {canceling} interrupt\n"),
                );
                self.io_intr_scan.store(false, Ordering::Relaxed);
                if self.average.is_none() && !self.asyn_readback {
                    self.interrupt_sub = None;
                }
            }
        }
        // C returns 0 whether or not the driver took the client.
        true
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        // C's `ioScanPvt`, for a record on I/O Intr or one whose callbacks run
        // in every SCAN (an average or an `asyn:READBACK` output). The client
        // feeding it is `get_ioint_info`'s, or `init`'s for those two.
        if !self.reason_set {
            return None;
        }

        // Time-series device support (asynInt32/Float64/Int64TimeSeries): the
        // wakeup the callback sends when the buffer fills (C
        // `callbackRequestProcessCallback`, devAsynXXXTimeSeries.h:202-212).
        // The callback itself is registered by process while BUSY
        // ([`Self::time_series_subscribe`]).
        if self.time_series.is_some() {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            self.time_series_wake = Some(tx);
            return Some(rx);
        }

        if self.average.is_none() && !self.is_io_intr_scan() && !self.asyn_readback {
            return None;
        }
        self.scan_io_rx.take()
    }

    /// An `asyn_readback`-flagged record follows driver-side changes via the
    /// interrupt callback regardless of its `SCAN` (upstream PRs #60 / #208).
    /// Decouple its poll-feedback wiring from the `SCAN` menu so the callback
    /// processes it even when `SCAN != "I/O Intr"`. Plain `SCAN="I/O Intr"`
    /// records (`asyn_readback == false`) keep the SCAN-gated behaviour.
    ///
    /// Averaging records (`average.is_some()`) also need the driver-callback
    /// path wired regardless of SCAN — the accumulating interrupt must run on
    /// every driver sample while the record scans periodically. C registers that
    /// callback in `initAiAverage` (devAsynInt32.c:870-872), outside the SCAN
    /// path entirely; `getIoIntInfo` skips it (`if (!pPvt->isAiAverage)`,
    /// :385-394), which is what makes it independent of SCAN.
    /// `init` armed the accumulating subscription and `io_intr_receiver`
    /// returns `None` off I/O Intr, so this spawns no reprocess task (the
    /// record scans on its own period, not per callback).
    fn io_intr_scan_independent(&self) -> bool {
        // Time-series records are SCAN="Passive" (driven by RARM puts, the Read
        // FLNK, and the completion callback) but must still receive the
        // accumulating callback regardless of SCAN — same as averaging.
        self.asyn_readback || self.average.is_some() || self.time_series.is_some()
    }

    fn arm_readback_callback(&mut self) -> DeviceUdf {
        // C `devAsynInt32.c::outputCallbackCallback` sets
        // `newOutputCallbackValue = 1` immediately before `dbProcess`. Only
        // the output-readback path (asyn:READBACK, PRs #60 / #208) carries
        // the C `newOutputCallbackValue` contract; input I/O-Intr records use
        // the scan/RPRO reprocess model (`processCommon`), never the discard
        // fallback, so they are left unarmed and `reconcile` is a no-op there.
        if self.asyn_readback {
            self.output_callback_pending = true;
            // ...and `pr->udf = 0` with it (devAsynInt32.c:631 and its twins),
            // so a failed readback raises WRITE_ALARM, not UDF_ALARM.
            return DeviceUdf::Defined;
        }
        DeviceUdf::Untouched
    }

    fn reconcile_readback_callback(&mut self) {
        // The pump's pass is over (`process_record_readback` is its only
        // caller), and with it the request it ran on.
        self.interrupt_fifo.lock().unwrap().answered();
        // C `outputCallbackCallback` after `dbProcess`:
        //   if (pPvt->newOutputCallbackValue != 0) { getCallbackValue(pPvt);
        //       pPvt->newOutputCallbackValue = 0; }
        // The flag still being set means the record never reached its read
        // stage (the PACT entry guard bailed because a put / FLNK cycle still
        // owned the record). Discard the oldest ring entry so the callback
        // ring stays balanced — every armed callback consumes exactly one
        // entry. `read()` already cleared the flag on the cycles that did
        // process, so this only fires on a genuine bail.
        if self.output_callback_pending {
            let dropped = self.interrupt_fifo.lock().unwrap().discard();
            if let Some(d) = &dropped {
                self.popped(d);
            }
            self.output_callback_pending = false;
        } else {
            // The cycle processed, so it reached the bottom of `processXxx`.
            self.end_processing();
        }
    }

    fn output_callback_readback(&self) -> bool {
        // The C `newOutputCallbackValue` contract applies exactly where
        // `arm_readback_callback` arms: asyn:READBACK outputs (PRs #60/#208),
        // whose callback cycle reads the driver value back and must not
        // re-write the setpoint. Records without the readback subscription
        // keep the full output stage on callback cycles, like any C
        // `dbProcess`.
        self.asyn_readback
    }

    fn property_post_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<PropertyPost>> {
        // Runtime asynEnum table re-propagation. C devAsynInt32.c registers a
        // per-record enum callback (callbackEnum, :712-766) that re-applies
        // `setEnums` + `db_post_events(DBE_PROPERTY)` whenever the driver
        // changes the table via `doCallbacksEnum`. Gate exactly as C's
        // `findInterface(asynEnumType) && maxEnums>0`: only a record that
        // exposed an enum state family on a port with asynEnum (so
        // `enum_shape` is set) arms the callback. Independent of SCAN —
        // a property post is not a value scan, so it never processes the
        // record.
        let shape = self.enum_shape?;

        let mut intr_rx = self.enum_intr_rx.take()?;

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut last_choices = self.enum_choices.clone();
        crate::runtime::task::spawn(async move {
            while let Some(iv) = intr_rx.recv().await {
                let crate::param::ParamValue::Enum { choices, .. } = &iv.value else {
                    continue;
                };
                let changed = match &last_choices {
                    Some(prev) => prev.as_ref() != choices.as_ref(),
                    None => true,
                };
                if !changed {
                    continue;
                }
                last_choices = Some(choices.clone());
                // C posts on VAL for every one of the eight callbacks —
                // mbbi/mbbo (`&pr->val` after a 16-state setEnums) and bi/bo
                // (2-state) alike — so the target is uniform across both
                // `EnumRecordShape`s and needs no per-shape branch.
                let post = PropertyPost {
                    writes: enum_table_fields(shape, choices),
                    post_field: "VAL".to_string(),
                };
                if tx.send(post).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

// ===== Universal asyn device support =====

/// Whether a DTYP name fixes the record as an output on its own, so that an
/// `@asyn` link in INP still means "write". C binds these dsets to
/// `&pwf->inp` with `isOutput=1`: every `asynXxxArrayWfOut`
/// (`MAKE_DEVSUP(asynInt8ArrayWfOut, waveformRecord, inp, ..., true)`,
/// devAsynXXXArray.cpp:475-505,512 and the Int16/32/64/Float32/Float64
/// siblings), `asynOctetWrite` and `asynOctetWriteBinary` (initWfWrite /
/// initWfWriteBinary, devAsynOctet.c:1065,1080). Every other DTYP takes its
/// direction from the link field it was given.
fn dtyp_is_output(dtyp: &str) -> bool {
    dtyp.ends_with("ArrayOut") || dtyp == "asynOctetWrite" || dtyp == "asynOctetWriteBinary"
}

/// Normalize array DTYP names by stripping "In"/"Out" direction suffixes.
///
/// C EPICS uses distinct DTYPs for input vs output array records
/// (e.g. `asynFloat64ArrayIn`, `asynFloat64ArrayOut`), but the underlying
/// asyn interface name is just `asynFloat64Array`. This function strips
/// the direction suffix so the adapter's `read_op()`/`write_op()` matchers
/// can find the correct interface.
/// Normalize asyn DTYP names to their base interface type.
///
/// C EPICS uses direction-specific DTYPs for some interfaces:
/// - `asynFloat64ArrayIn` / `asynFloat64ArrayOut` → `asynFloat64Array`
/// - `asynOctetRead` / `asynOctetWrite` → `asynOctet`
///
/// The underlying asyn interface is direction-agnostic.
fn normalize_asyn_dtyp(dtyp: &str) -> String {
    // Array direction suffixes: asynXxxArrayIn/Out → asynXxxArray
    if let Some(base) = dtyp.strip_suffix("In").or_else(|| dtyp.strip_suffix("Out")) {
        if base.ends_with("Array") {
            return base.to_string();
        }
    }
    // Octet direction suffixes: asynOctetRead/Write → asynOctet.
    // asynOctetCmdResponse (write-then-read, carried by `octet_cmd`) and
    // asynOctetWriteBinary (write-only, no-NUL-trim, carried by `octet_binary`)
    // also resolve to the asynOctet interface — C devAsynOctet.c registers every
    // one of these dsets on asynOctet; the direction/behavior is record state,
    // not a distinct interface.
    if dtyp == "asynOctetRead"
        || dtyp == "asynOctetWrite"
        || dtyp == "asynOctetWriteBinary"
        || dtyp == "asynOctetCmdResponse"
        || dtyp == "asynOctetWriteRead"
    {
        return "asynOctet".to_string();
    }
    // Averaging DTYPs use the base interface for driver I/O; averaging is a
    // device-support behaviour carried by `average`, not a distinct interface
    // (C `asynInt32Average` registers on asynInt32, `asynFloat64Average` on
    // asynFloat64 — devAsynInt32.c:870-872 / devAsynFloat64.c:687-694).
    if dtyp == "asynInt32Average" {
        return "asynInt32".to_string();
    }
    if dtyp == "asynFloat64Average" {
        return "asynFloat64".to_string();
    }
    // Time-series DTYPs likewise use the base interface for driver I/O; the
    // scalar-into-array accumulation is a device-support behaviour carried by
    // `time_series` (C `asynInt32TimeSeries` registers on asynInt32, etc. —
    // devAsynXXXTimeSeries.h INTERFACE_TYPE: asynInt32Type/asynFloat64Type/
    // asynInt64Type).
    if dtyp == "asynInt32TimeSeries" {
        return "asynInt32".to_string();
    }
    if dtyp == "asynFloat64TimeSeries" {
        return "asynFloat64".to_string();
    }
    if dtyp == "asynInt64TimeSeries" {
        return "asynInt64".to_string();
    }
    dtyp.to_string()
}

/// The C dset a record's DTYP binds, which names every line it prints: its
/// `initCommon` refusals and the lines a request prints.
/// Which devAsyn `registerInterruptUser` a registration is, for its
/// failure line ([`AsynDeviceSupport::report_register_refusal`]).
#[derive(Clone, Copy)]
enum Register {
    /// `initCommon`'s, for an `asyn:READBACK` output.
    ReadBack,
    /// `getIoIntInfo`'s, for an `I/O Intr` record.
    IoIntr,
    /// `initAiAverage`'s.
    Average,
    /// `initCommon`'s on asynEnum.
    Enum,
    /// The time-series `process`'s on BUSY 0->1.
    TimeSeries,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dset {
    /// devAsynInt32.c:220-230 and devBusyAsyn.c:158-167: `parseLink`, then
    /// `parseLinkMask`, whose message is the one left to print.
    Int32 {
        busy: bool,
    },
    UInt32Digital,
    Float64,
    Int64,
    Octet,
    Array,
    TimeSeries(&'static str),
}

impl Dset {
    fn of(record_type: &str, dtyp: &str) -> Self {
        match dtyp {
            "asynInt32TimeSeries" => Self::TimeSeries("devAsynInt32TimeSeries"),
            "asynInt64TimeSeries" => Self::TimeSeries("devAsynInt64TimeSeries"),
            "asynFloat64TimeSeries" => Self::TimeSeries("devAsynFloat64TimeSeries"),
            d if d.contains("Array") => Self::Array,
            d if d.starts_with("asynOctet") => Self::Octet,
            d if d.starts_with("asynUInt32Digital") => Self::UInt32Digital,
            d if d.starts_with("asynInt64") => Self::Int64,
            d if d.starts_with("asynFloat64") => Self::Float64,
            _ => Self::Int32 {
                busy: record_type == "busy",
            },
        }
    }

    /// The link's fields, with `Some(mask)` when it parsed as `@asynMask`.
    fn parse(self, link: &str) -> Result<(InstIo, Option<u32>), &'static str> {
        let plain = |link| parse_inst_io(link, false).map(|io| (io, None));
        let mask = |link| {
            parse_inst_io(link, true).map(|io| {
                let mask = io.mask;
                (io, Some(mask))
            })
        };
        match self {
            Self::Int32 { .. } => plain(link).or_else(|_| mask(link)),
            Self::UInt32Digital => mask(link),
            _ => plain(link),
        }
    }

    /// The dsets whose requests print `processCallbackOutput`/`Input` lines.
    fn is_scalar(self) -> bool {
        matches!(
            self,
            Self::Int32 { .. } | Self::Int64 | Self::UInt32Digital | Self::Float64
        )
    }

    /// The C dset's `driverName`.
    fn driver(self) -> &'static str {
        match self {
            Self::Int32 { busy: true } => "devBusyAsyn",
            Self::Int32 { busy: false } => "devAsynInt32",
            Self::UInt32Digital => "devAsynUInt32Digital",
            Self::Float64 => "devAsynFloat64",
            Self::Int64 => "devAsynInt64",
            Self::Octet => "devAsynOctet",
            Self::Array => "devAsynXXXArray",
            Self::TimeSeries(driver) => driver,
        }
    }

    /// C's line for a refused `registerInterruptUser`, and whether C prints it
    /// at `ASYN_TRACE_ERROR` (`true`) rather than with `printf`.
    fn register_refusal_line(
        self,
        at: Register,
        rec: &str,
        msg: &dyn std::fmt::Display,
    ) -> (String, bool) {
        let driver = self.driver();
        match (at, self) {
            (Register::ReadBack, dset) => {
                let at = match dset {
                    Self::Float64 => "initRecord",
                    Self::Array => "devAsynXXXArray",
                    _ => "initCommon",
                };
                let line =
                    format!("{rec} {driver}::{at} error calling registerInterruptUser {msg}");
                (line, false)
            }
            (Register::IoIntr, Self::Octet | Self::Array) => (
                format!("{rec} {driver}::getIoIntInfo error calling registerInterruptUser {msg}"),
                self == Self::Array,
            ),
            (Register::IoIntr, _) => (
                format!("{rec} {driver}::getIoIntInfo registerInterruptUser {msg}"),
                false,
            ),
            (Register::Average, _) => (
                format!("{rec} {driver}::initAiAverage registerInterruptUser {msg}"),
                false,
            ),
            (Register::Enum, _) => (
                format!("{rec} {driver}::initCommon enum registerInterruptUser {msg}"),
                false,
            ),
            (Register::TimeSeries, _) => {
                (format!("{rec} {driver} registerInterruptUser {msg}"), true)
            }
        }
    }

    /// Print C's line for a refusal where C prints it.
    fn report(self, record: &str, refusal: &LinkRefusal) {
        match self.refusal_line(record, refusal) {
            (line, true) => epics_base_rs::runtime::log::errlog_vprintf(format_args!("{line}\n")),
            (line, false) => epics_libcom_rs::epics_println!("{line}"),
        }
    }

    /// The record after C's `bad:` arm.
    fn bad(self) -> DeviceInitOutcome {
        match self {
            // devAsynXXXTimeSeries.h:118-120 raises no alarm.
            Self::TimeSeries(_) => DeviceInitOutcome::dead(),
            _ => DeviceInitOutcome::dead_with_alarm(
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid,
            ),
        }
    }

    /// A time-series record's buffer for its FTVL. C checks FTVL first in
    /// `initRecord` (devAsynXXXTimeSeries.h:73-77): the signed or unsigned
    /// type of the interface, else errlog and `goto bad`. menuFtype indices:
    /// LONG=5, ULONG=6, INT64=7, UINT64=8, DOUBLE=10. `Ok(None)` for every
    /// other dset.
    fn check_ftvl(
        self,
        name: &str,
        record: &dyn Record,
    ) -> Result<Option<TsBuf>, DeviceInitOutcome> {
        let Self::TimeSeries(driver) = self else {
            return Ok(None);
        };
        let ftvl = match record.get_field("FTVL") {
            Some(EpicsValue::Short(v)) => v,
            _ => -1,
        };
        let buf = match (driver, ftvl) {
            ("devAsynInt32TimeSeries", 5 | 6) => TsBuf::Long(Vec::new()),
            ("devAsynInt64TimeSeries", 7) => TsBuf::Int64(Vec::new()),
            ("devAsynInt64TimeSeries", 8) => TsBuf::UInt64(Vec::new()),
            ("devAsynFloat64TimeSeries", 10) => TsBuf::Double(Vec::new()),
            _ => {
                epics_base_rs::runtime::log::errlog_vprintf(format_args!(
                    "{driver}::initCommon, {name} field type must be SIGNED_TYPE or UNSIGNED_TYPE\n"
                ));
                return Err(self.bad());
            }
        };
        Ok(Some(buf))
    }

    /// C's line for an `initCommon` refusal, and whether C sends it through
    /// errlog (`true`) or `printf`.
    fn refusal_line(self, record: &str, refusal: &LinkRefusal) -> (String, bool) {
        let driver = self.driver();
        match refusal {
            LinkRefusal::Parse(message) => match self {
                Self::Int32 { .. } | Self::Int64 => {
                    (format!("{record} {driver}::initCommon  {message}"), false)
                }
                Self::UInt32Digital | Self::Float64 => {
                    (format!("{record} {driver}::initCommon {message}"), false)
                }
                Self::Octet => (
                    format!("{record} {driver}::initCommon error in link {message}"),
                    false,
                ),
                Self::Array => (
                    format!("{driver}::devAsynXXXArray, {record} error in link {message}"),
                    true,
                ),
                Self::TimeSeries(_) => (
                    format!("{driver}::initCommon, {record} error in link {message}"),
                    true,
                ),
            },
            LinkRefusal::DrvUser(message) => match self {
                Self::Octet => (
                    format!("{record} {driver}::initDrvUser drvUserCreate failed {message}"),
                    false,
                ),
                Self::Array => (
                    format!("{driver}::devAsynXXXArray, {record} drvUserCreate failed {message}"),
                    true,
                ),
                Self::TimeSeries(_) => (
                    format!("{driver}::initCommon, {record} drvUserCreate failed {message}"),
                    true,
                ),
                _ => (
                    format!("{record} {driver}::initCommon drvUserCreate {message}"),
                    false,
                ),
            },
            LinkRefusal::NoPort(port) => {
                let message = format!("asynManager:connectDevice port {port} not found");
                match self {
                    Self::Array => (
                        format!(
                            "{driver}::devAsynXXXArray, {record} connectDevice failed {message}"
                        ),
                        true,
                    ),
                    Self::TimeSeries(_) => (
                        format!("{driver}::initCommon, {record} connectDevice failed {message}"),
                        true,
                    ),
                    _ => (
                        format!("{record} {driver}::initCommon connectDevice failed {message}"),
                        false,
                    ),
                }
            }
            LinkRefusal::SyncIo(sync_io, message) => (
                format!("{record} {driver}::initCommon {sync_io}->connect failed {message}"),
                false,
            ),
        }
    }

    /// The SyncIO C `initCommon` connects right after `findInterface`.
    fn sync_io(self) -> Option<&'static str> {
        match self {
            Self::Int32 { .. } => Some("Int32SyncIO"),
            Self::Int64 => Some("Int64SyncIO"),
            Self::UInt32Digital => Some("UInt32DigitalSyncIO"),
            Self::Float64 => Some("Float64SyncIO"),
            _ => None,
        }
    }

    /// Whether C `initCommon` reads a driver enum table, through an
    /// EnumSyncIO connect, for a record with enum states.
    fn connects_enum_sync_io(self) -> bool {
        matches!(self, Self::Int32 { .. } | Self::UInt32Digital)
    }
}

/// Why a dset's `initCommon` refused the record's link.
enum LinkRefusal {
    /// `parseLink`/`parseLinkMask` failed with this `errorMessage`.
    Parse(&'static str),
    /// `connectDevice` found no port of this name.
    NoPort(String),
    /// `drvUserCreate` failed with this `errorMessage`.
    DrvUser(String),
    /// This SyncIO's `connect` failed with this `errorMessage`.
    SyncIo(&'static str, String),
}

/// What `pasynUser->errorMessage` holds after a failed `drvUserCreate`.
/// asynPortDriver reports a missing parameter by its trace line alone
/// (asynPortDriver.cpp:3571-3575) and leaves the message empty.
fn drv_user_message(e: &AsynError) -> String {
    match e {
        AsynError::ParamNotFound(_) => String::new(),
        e => e.message(),
    }
}

/// A record whose link C's dset refused: the dset is bound, and its
/// `init_record` prints why and takes the `bad:` arm.
struct BadLink {
    dset: Dset,
    dtyp: String,
    refusal: LinkRefusal,
    record: String,
}

impl DeviceSupport for BadLink {
    fn dtyp(&self) -> &str {
        &self.dtyp
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.record = name.to_string();
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        if let Err(dead) = self.dset.check_ftvl(&self.record, record) {
            return Ok(dead);
        }
        self.dset.report(&self.record, &self.refusal);
        Ok(self.dset.bad())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

fn bad_link(dset: Dset, dtyp: &str, refusal: LinkRefusal) -> Box<dyn DeviceSupport> {
    Box::new(BadLink {
        dset,
        dtyp: dtyp.to_string(),
        refusal,
        record: String::new(),
    })
}

/// Create a universal asyn device support factory.
///
/// Handles all standard asyn DTYPs (`asynInt32`, `asynFloat64`, `asynOctet`,
/// array types, etc.) by parsing `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links
/// and dispatching to the appropriate port driver.
///
/// During `init()`, `drv_user_create(drvInfo)` is called on the port, which
/// resolves the drvInfo string to a param index via `find_param()`. This
/// matches the C EPICS asyn device support behavior exactly.
///
/// Handles all records with `@asyn(PORT,...)` links. Register via
/// `register_asyn_device_support(app)` or `AdIoc` (which registers it
/// automatically).
///
/// ```ignore
/// app = asyn_rs::adapter::register_asyn_device_support(app);
/// ```
pub fn universal_asyn_factory(
    ctx: &epics_base_rs::server::ioc_app::DeviceSupportContext,
) -> Option<Box<dyn DeviceSupport>> {
    // The asyn record's own DSET (C `devAsynRecord.dbd`:
    // `device(asyn, INST_IO, asynRecordDevice, "asynRecordDevice")`). It binds
    // no link — the record carries PORT/ADDR/DRVINFO as fields — so it must be
    // recognised before the `@asyn(...)` parse below, which would reject it.
    if ctx.dtyp == crate::asyn_record::ASYN_RECORD_DTYP {
        return Some(Box::new(crate::asyn_record::AsynRecordDevice::new()));
    }

    // C declares each dset per record type (`device(ao, INST_IO, asynAoInt32,
    // "asynInt32")`), and a DTYP put admits only a choice of THAT record
    // type's device menu: `dbPutStringNum` answers `S_db_badChoice` "no such
    // device support for 'calcout' record type" (dbStaticRun.c:485-502), so
    // `record(calcout) { field(DTYP, "asynInt32") }` fails at db load with
    // `Can't set 'X.DTYP' to 'asynInt32'` (dbLexRoutines.c:1405-1414). This
    // factory stands in for every one of those dsets at once, so it makes
    // the same refusal here: a pair no `device()` line declares gets no
    // device, and base reports the record as having no device support,
    // instead of binding a dset the record type never calls (a calcout
    // never invokes write(), so the record loaded, computed OVAL and wrote
    // nothing to the driver).
    if !asyn_device_menu(ctx.record_type)
        .is_some_and(|menu| menu.iter().any(|row| row.choice == ctx.dtyp))
    {
        return None;
    }

    // C device support reads the record's own link: OUT on an output record,
    // INP otherwise. An INP link is an output only when the DTYP names the
    // direction.
    let (link_str, is_output, output_via_inp) = if !ctx.out.trim().is_empty() {
        (ctx.out, true, false)
    } else {
        let out = dtyp_is_output(ctx.dtyp);
        (ctx.inp, out, out)
    };

    let dset = Dset::of(ctx.record_type, ctx.dtyp);
    let (link, mask) = match dset.parse(link_str) {
        Ok((io, mask)) => (
            AsynLink {
                port_name: io.port_name,
                addr: io.addr,
                timeout: io.timeout,
                drv_info: io.drv_info,
            },
            mask,
        ),
        Err(message) => return Some(bad_link(dset, ctx.dtyp, LinkRefusal::Parse(message))),
    };

    let Some(entry) = crate::asyn_record::get_port(&link.port_name) else {
        return Some(bad_link(
            dset,
            ctx.dtyp,
            LinkRefusal::NoPort(link.port_name),
        ));
    };

    // Normalize DTYP: strip "In"/"Out" suffixes from array types.
    // C EPICS uses DTYPs like "asynFloat64ArrayIn" / "asynFloat64ArrayOut"
    // but the underlying asyn interface is "asynFloat64Array".
    let dtyp = normalize_asyn_dtyp(ctx.dtyp);

    let mut adapter = AsynDeviceSupport::from_handle(entry.handle, link, &dtyp);

    // asynOctetCmdResponse: the DRVINFO is a LITERAL command, not a param name
    // (C `asynSiOctetCmdResponse` passes useDrvUser=0 → no drvUserCreate). Escape-
    // translate it once (C `initCmdBuffer`, devAsynOctet.c) and cache it; each
    // process writes the command then reads the reply into VAL (`read_op` emits
    // `OctetWriteRead`). Pre-set `reason_set` so `init()` skips the param
    // resolution that would fail on a command string — reason stays 0, and octet
    // I/O keys off the addr, not the reason.
    if ctx.dtyp == "asynOctetCmdResponse" {
        // C initCmdBuffer (devAsynOctet.c:639-641) runs dbTranslateEscape then
        // bufLen = strlen(buffer): an escaped NUL (\0 / \000) terminates the
        // command, so only the bytes up to the first NUL are written. Truncate
        // at the first NUL to match the bytes C actually puts on the wire. The
        // truncated command may be empty (a leading-NUL command, strlen 0); that
        // is NOT a misconfiguration — C still writes 0 bytes then reads. Only a
        // raw-empty DRVINFO is rejected, and that is gated in read() on the raw
        // drv_info (C `strlen(userParam)`), not on this truncated command.
        let mut cmd = crate::asyn_record::translate_escape(&adapter.drv_info);
        if let Some(nul) = cmd.iter().position(|&b| b == 0) {
            cmd.truncate(nul);
        }
        adapter.octet_cmd = Some(cmd);
        adapter.reason_set = true;
    }

    // asynOctetWriteRead: the DRVINFO names the record whose value is the
    // command (C `initSiWriteRead` and kin, useDrvUser=0, devAsynOctet.c:866-875).
    if ctx.dtyp == "asynOctetWriteRead" {
        let _ = adapter.device_job.write_read.set(WriteReadSource {
            pv: adapter.drv_info.clone(),
            waveform: ctx.record_type == "waveform",
            db: std::sync::OnceLock::new(),
        });
        adapter.reason_set = true;
    }

    // asynInt32Average / asynFloat64Average: averaging device support. C
    // `initAiAverage` (devAsynInt32.c:858-885, devAsynFloat64.c:687-706) registers an
    // always-on accumulating interrupt; the periodic record process drains
    // the mean. The DRVINFO is a real param (useDrvUser=1), so `reason_set`
    // is left false and `init()` runs `drv_user_create` as for a plain
    // asynInt32/asynFloat64 ai. The accumulating subscription is registered
    // in `io_intr_receiver` (which `io_intr_scan_independent` enables here).
    if ctx.dtyp == "asynInt32Average" || ctx.dtyp == "asynFloat64Average" {
        adapter.average = Some(Arc::new(AverageState::new()));
    }

    // asynInt32TimeSeries / asynFloat64TimeSeries / asynInt64TimeSeries:
    // time-series device support (waveform-only). The DRVINFO is a real param
    // (useDrvUser=1), so `reason_set` is left false and `init()` runs
    // `drv_user_create` as for a plain scalar record; init also validates FTVL
    // and sizes the buffer from NELM. The accumulating subscription is armed in
    // `io_intr_receiver` (which `io_intr_scan_independent` enables here).
    if ctx.dtyp == "asynInt32TimeSeries"
        || ctx.dtyp == "asynFloat64TimeSeries"
        || ctx.dtyp == "asynInt64TimeSeries"
    {
        adapter.time_series = Some(Arc::new(TimeSeriesState::new()));
    }

    adapter.output = is_output;
    if is_output {
        if output_via_inp {
            // An output DTYP bound through INP sits on a waveform, an input
            // record type whose process calls read(): the write runs there
            // (`write_only`), and no read ever replaces VAL — for asynOctet it
            // would turn the CharArray into a String and break element_count.
            // C registers no ASYN_INIT readback for any of these dsets
            // (devAsynXXXArray.cpp:97-200, devAsynOctet.c initWfWrite).
            adapter.write_only = true;
            // asynOctetWriteBinary writes the full NORD bytes with NO NUL-trim
            // (C callbackWfWriteBinary, devAsynOctet.c:1086-1091), unlike
            // asynOctetWrite which trims at the first NUL (my_strnlen, :1071-1076).
            adapter.octet_binary = ctx.dtyp == "asynOctetWriteBinary";
        } else if dtyp != "asynOctet" {
            // Output records: read back current driver value on init.
            // Mirrors C `initAo` / `initBo` / `initLongout` / `initMbbo`
            // which call `pasynManager->queueRequest(... ASYN_INIT ...)`
            // to pull the driver's current value into the record before
            // record processing starts. NOT asynOctet: C devAsynOctet reads
            // an output's initial value only under info(asyn:INITIAL_READBACK,
            // "1") (devAsynOctet.c:357-380; `initSoWrite` queues no ASYN_INIT
            // read), so a stringout's .db VAL — NDPluginBase.template's
            // NDArrayPort macro among them — survives a driver param that a
            // plugin constructor seeded with "". The tag path stays in
            // `apply_record_info`.
            adapter = adapter.with_initial_readback();
        }
    }
    // Input records: do NOT auto-readback. C `initAi`/`initLongin`/
    // `initMbbi`/`initBi` (devAsynInt32.c:812+, similar for Float64 /
    // Int64 / UInt32Digital / Octet) only sets up the asynUser and
    // gets bounds — the first value comes from `processAi` (driven by
    // the scan task or an I/O Intr callback), not from a synchronous
    // read at init. The previous "matches C init_common() behavior"
    // comment was wrong: C devAsynOctet init_common() also only
    // installs callbacks; the initial value flows through the scan
    // path. Auto-readback on inputs caused two problems: a spurious
    // blocking read against an unconnected driver, and overwriting a
    // template's deliberate default value with a stale/zero readback.

    // A plain `@asyn` link leaves C's mask 0: no mask.
    if let Some(mask) = mask {
        adapter = adapter.with_mask(mask);
    }

    Some(Box::new(adapter))
}

/// Register universal asyn device support on an IocApplication.
///
/// This is the Rust equivalent of C EPICS's standard asyn device support
/// registration. Call this BEFORE registering plugin or driver-specific
/// factories so they take precedence (dynamic factories chain last-registered-first).
///
/// It is also what brings the asyn iocsh commands: loading `asyn.dbd` runs
/// the `asynRegister` registrar (asynShellCommands.c:1349-1382) beside the
/// `device()` lines, so a C IOC that has asyn device support has
/// `asynSetTraceMask`, `asynReport`, `drvAsynIPPortConfigure` and the rest
/// with no further step. They act on [`crate::manager::PortManager::global`], the port table
/// every port built with `RuntimeConfig::default()` traces through. An IOC
/// that registered the commands itself on a manager it built kept a table
/// the drivers' ports were not in — `asynSetTraceMask` on such a port set a
/// mask nothing read — and an IOC that did not was missing the commands.
pub fn register_asyn_device_support(
    app: epics_base_rs::server::ioc_app::IocApplication,
) -> epics_base_rs::server::ioc_app::IocApplication {
    let app = app.register_dynamic_device_support(&asyn_devices(), universal_asyn_factory);
    crate::iocsh::register_asyn_commands(app, crate::manager::PortManager::global())
}

/// Contribute asyn's device-support DTYP menus to base's `DTYP` choice lists.
///
/// Walks the generated [`crate::dbd_generated::DEVICE_MENU_RECORD_TYPES`] and
/// hands each record type's asyn menu to base's
/// [`register_device_menu`](epics_base_rs::server::record::register_device_menu),
/// so a client reading e.g. `mbbo.DTYP` sees the `asynInt32` /
/// `asynUInt32Digital` choices a C fat `softIoc` lists after loading
/// `asyn.dbd`. Without this, base serves only its own `device()` lines (Soft
/// Channel, ...) plus the record's currently-bound DTYP, omitting every asyn
/// DTYP the record is not itself bound to.
///
/// Idempotent: base dedups a repeat registration, so the
/// `register_asyn_device_support*` entry points may each call it. It registers
/// into a process-global table (the menu is per record TYPE, as in C), so one
/// call suffices for every IOC in the process.
pub fn register_asyn_device_menus() {
    for &record_type in ASYN_DEVICE_MENU_RECORD_TYPES {
        if let Some(rows) = asyn_device_menu(record_type) {
            epics_base_rs::server::record::register_device_menu(record_type, rows);
        }
    }
}

/// Every record type's [`asyn_device_menu`] rows, the `device()` lines
/// [`universal_asyn_factory`] serves.
fn asyn_devices() -> Vec<(&'static str, &'static [DevSup])> {
    ASYN_DEVICE_MENU_RECORD_TYPES
        .iter()
        .filter_map(|&record_type| Some((record_type, asyn_device_menu(record_type)?)))
        .collect()
}

/// The record types [`asyn_device_menu`] answers for: the generated table plus
/// `busy`.
static ASYN_DEVICE_MENU_RECORD_TYPES: &[&str] = &{
    const N: usize = crate::dbd_generated::DEVICE_MENU_RECORD_TYPES.len() + 1;
    let mut all = [""; N];
    let mut i = 0;
    while i < N - 1 {
        all[i] = crate::dbd_generated::DEVICE_MENU_RECORD_TYPES[i];
        i += 1;
    }
    all[N - 1] = "busy";
    all
};

/// The asyn DTYP choices `device()` declares for a record type — the single
/// source both [`register_asyn_device_menus`] (what base lists) and
/// [`universal_asyn_factory`] (what it agrees to bind) read, so the two can
/// never disagree. `None` for a record type asyn's `.dbd` never names.
///
/// The busy record's asyn binding comes from the busy MODULE, not asyn's own
/// .dbd (busySupport_withASYN.dbd: `device(busy,INST_IO,asynBusyInt32,
/// "asynInt32")`), so it is added here rather than in the generated table.
/// The binding itself is the universal factory + the unconditional readback
/// forced in `init` (devBusyAsyn.c parity).
fn asyn_device_menu(record_type: &str) -> Option<&'static [DevSup]> {
    if record_type == "busy" {
        return Some(BUSY_DEVICES);
    }
    crate::dbd_generated::devices(record_type)
}

static BUSY_DEVICES: &[DevSup] = &[DevSup {
    name: "asynBusyInt32",
    choice: "asynInt32",
    link_type: epics_base_rs::server::record::DbLinkType::InstIo,
    dset: crate::dsets::ASYNBUSYINT32,
}];

/// IocBuilder companion to [`register_asyn_device_support`] —
/// installs the universal asyn factory on the pure-Rust build path
/// (added `register_dynamic_device_support` to IocBuilder). An
/// `IocBuilder` has no command table, so the iocsh commands are the
/// `IocApplication` path's alone.
/// Without this helper, callers using `IocBuilder` instead of
/// `IocApplication` would have to wire `universal_asyn_factory`
/// manually; that asymmetry is exactly what `register_asyn_device_support`
/// hides for the IocApplication path.
pub fn register_asyn_device_support_for_builder(
    builder: epics_base_rs::server::ioc_builder::IocBuilder,
) -> epics_base_rs::server::ioc_builder::IocBuilder {
    builder.register_dynamic_device_support(&asyn_devices(), universal_asyn_factory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_actor::ActorLink;

    /// C's text for a refused `registerInterruptUser`, file by file
    /// (devAsynInt32.c:339/:391/:875/:322, devAsynFloat64.c:228/:282,
    /// devAsynOctet.c:439, devAsynXXXArray.cpp:182/:246,
    /// devAsynXXXTimeSeries.h:163).
    #[test]
    fn a_refused_register_prints_cs_line() {
        let msg =
            "asynManager:getInterruptPvt Driver does not support interrupts on interface asynInt32";
        let cases = [
            (
                Dset::Int32 { busy: false },
                Register::IoIntr,
                "R devAsynInt32::getIoIntInfo registerInterruptUser",
                false,
            ),
            (
                Dset::Int32 { busy: false },
                Register::ReadBack,
                "R devAsynInt32::initCommon error calling registerInterruptUser",
                false,
            ),
            (
                Dset::Int32 { busy: false },
                Register::Average,
                "R devAsynInt32::initAiAverage registerInterruptUser",
                false,
            ),
            (
                Dset::Int32 { busy: false },
                Register::Enum,
                "R devAsynInt32::initCommon enum registerInterruptUser",
                false,
            ),
            (
                Dset::Float64,
                Register::ReadBack,
                "R devAsynFloat64::initRecord error calling registerInterruptUser",
                false,
            ),
            (
                Dset::Octet,
                Register::IoIntr,
                "R devAsynOctet::getIoIntInfo error calling registerInterruptUser",
                false,
            ),
            (
                Dset::Array,
                Register::IoIntr,
                "R devAsynXXXArray::getIoIntInfo error calling registerInterruptUser",
                true,
            ),
            (
                Dset::Array,
                Register::ReadBack,
                "R devAsynXXXArray::devAsynXXXArray error calling registerInterruptUser",
                false,
            ),
            (
                Dset::TimeSeries("devAsynInt32TimeSeries"),
                Register::TimeSeries,
                "R devAsynInt32TimeSeries registerInterruptUser",
                true,
            ),
        ];
        for (dset, at, head, trace) in cases {
            assert_eq!(
                dset.register_refusal_line(at, "R", &msg),
                (format!("{head} {msg}"), trace),
                "{head}"
            );
        }
    }

    /// C asyn `init*` status per output record: 0 (convert) only where the
    /// dset returns it after a successful read back, 2 on every miss.
    #[test]
    fn init_status_matches_c_asyn_init_return() {
        use epics_base_rs::server::device_support::DeviceUdf::{Defined, Untouched};
        let ok = DeviceInitOutcome::ok();
        let no_convert = DeviceInitOutcome::DoNotConvert(Untouched);
        let cases = [
            ("ao", "asynInt32", true, ok),
            (
                "ao",
                "asynFloat64",
                true,
                DeviceInitOutcome::DoNotConvert(Defined),
            ),
            ("ao", "asynInt64", true, no_convert),
            ("bo", "asynUInt32Digital", true, ok),
            ("mbbo", "asynInt32", true, ok),
            (
                "mbboDirect",
                "asynUInt32Digital",
                true,
                DeviceInitOutcome::DoNotConvert(Defined),
            ),
            ("ao", "asynInt32", false, no_convert),
            ("bo", "asynInt32", false, no_convert),
            ("mbbo", "asynUInt32Digital", false, no_convert),
            ("mbboDirect", "asynInt32", false, no_convert),
            ("longout", "asynInt32", true, DeviceInitOutcome::Ok(Defined)),
            (
                "stringout",
                "asynOctetWrite",
                true,
                DeviceInitOutcome::Ok(Defined),
            ),
            ("longout", "asynInt32", false, ok),
            ("ai", "asynInt32", false, ok),
        ];
        for (rtype, iface, read_back, want) in cases {
            assert_eq!(
                init_status(rtype, iface, read_back),
                want,
                "{rtype} {iface} {read_back}"
            );
        }
    }

    /// The generated asyn device menus match the C `asyn.dbd` load, in the exact
    /// order (the DTYP index is wire-visible): `mbbo` gets `asynInt32` then
    /// `asynUInt32Digital` (devAsynInt32.dbd before devAsynUInt32Digital.dbd in
    /// `devEpics.dbd`), `stringin` the three `devAsynOctet.dbd` octet devices in
    /// declaration order.
    #[test]
    fn generated_device_menus_match_asyn_dbd_order() {
        assert_eq!(
            crate::dbd_generated::device_menu("mbbo"),
            Some(&["asynInt32", "asynUInt32Digital"][..]),
        );
        assert_eq!(
            crate::dbd_generated::device_menu("stringin"),
            Some(
                &[
                    "asynOctetCmdResponse",
                    "asynOctetWriteRead",
                    "asynOctetRead"
                ][..]
            ),
        );
        // waveform draws from several files; the octet devices (devAsynOctet.dbd,
        // included first) precede the array devices (devAsynXXXArray.dbd).
        let wf = crate::dbd_generated::device_menu("waveform").expect("waveform has asyn devices");
        assert_eq!(wf[0], "asynOctetCmdResponse");
        assert_eq!(wf[5], "asynInt8ArrayIn");
        // The asyn record type's own device support is base's, not asyn-rs's —
        // excluded from the vendored dbd so the merge does not double it.
        assert_eq!(crate::dbd_generated::device_menu("asyn"), None);
    }

    /// `register_asyn_device_menus` walks every generated record type and hands
    /// its menu to base without panicking; repeat calls are idempotent (base
    /// dedups), so wiring device support twice in a process is safe.
    #[test]
    fn register_asyn_device_menus_is_callable_and_idempotent() {
        register_asyn_device_menus();
        register_asyn_device_menus();
        // Every listed record type must resolve to a menu (the registration loop
        // relies on this invariant).
        for &rt in crate::dbd_generated::DEVICE_MENU_RECORD_TYPES {
            assert!(
                crate::dbd_generated::device_menu(rt).is_some(),
                "DEVICE_MENU_RECORD_TYPES lists {rt} but device_menu returns None"
            );
        }
    }

    /// Every asyn read error maps to the C `asynStatusToEpicsAlarm` pair
    /// under the device-support READ-path defaults (READ_ALARM / INVALID).
    /// Condition codes per epics-base alarm.h: READ=1, COMM=9, TIMEOUT=10,
    /// HW_LIMIT=11, DISABLE=18; INVALID severity = 3.
    #[test]
    fn asyn_error_to_alarm_matches_c_status_to_epics_alarm() {
        use crate::error::AsynStatus;
        let mk = |s: AsynStatus| {
            asyn_error_to_alarm(&AsynError::Status {
                status: s,
                message: String::new(),
            })
        };
        assert_eq!(mk(AsynStatus::Success), (0, 0));
        assert_eq!(
            mk(AsynStatus::Timeout),
            (10, 3),
            "Timeout -> TIMEOUT_ALARM(10), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Overflow),
            (11, 3),
            "Overflow -> HW_LIMIT_ALARM(11), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Disconnected),
            (9, 3),
            "Disconnected -> COMM_ALARM(9), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Disabled),
            (18, 3),
            "Disabled -> DISABLE_ALARM(18), INVALID(3)"
        );
        assert_eq!(
            mk(AsynStatus::Error),
            (1, 3),
            "Error -> READ_ALARM(1), INVALID(3)"
        );
        // Non-status asyn errors take C's asynError/default branch.
        assert_eq!(
            asyn_error_to_alarm(&AsynError::PortNotFound("p".into())),
            (1, 3)
        );
    }

    /// Output (write-only) device support maps a transfer failure with C's
    /// WRITE_ALARM default (processCommon: `isOutput ? WRITE_ALARM : READ_ALARM`,
    /// devAsynOctet.c:806-807). Only the generic-`asynError` arm and the
    /// non-status arm change with direction; the specific transport statuses
    /// (Timeout/Overflow/Disconnected/Disabled) map direction-independently.
    #[test]
    fn asyn_error_to_alarm_write_default_is_write_alarm() {
        use crate::error::AsynStatus;
        use epics_base_rs::server::recgbl::alarm_status::WRITE_ALARM;
        let mk = |s: AsynStatus| {
            asyn_error_to_alarm_with_default(
                &AsynError::Status {
                    status: s,
                    message: String::new(),
                },
                WRITE_ALARM,
            )
        };
        // Generic asynError -> the direction default = WRITE_ALARM(2), INVALID(3).
        assert_eq!(
            mk(AsynStatus::Error),
            (2, 3),
            "output asynError default => WRITE_ALARM(2), not READ_ALARM(1)"
        );
        // Non-status error also takes the direction default.
        assert_eq!(
            asyn_error_to_alarm_with_default(&AsynError::PortNotFound("p".into()), WRITE_ALARM),
            (2, 3)
        );
        // Specific statuses stay direction-independent.
        assert_eq!(
            mk(AsynStatus::Timeout),
            (10, 3),
            "Timeout independent of dir"
        );
        assert_eq!(
            mk(AsynStatus::Disconnected),
            (9, 3),
            "Disconnected independent of dir"
        );
        assert_eq!(mk(AsynStatus::Success), (0, 0));
    }

    /// Regression: the IocBuilder companion helper exists
    /// and accepts a pure-Rust builder. Pre-fix, `register_asyn_device_support`
    /// only accepted IocApplication, so an IocBuilder caller had to
    /// expose `universal_asyn_factory` themselves.
    #[tokio::test]
    async fn register_asyn_device_support_for_builder_compiles_and_attaches() {
        use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use epics_base_rs::server::ioc_builder::IocBuilder;
        use epics_base_rs::server::record::ScanType;
        use epics_base_rs::types::EpicsValue;
        let _ = (
            ScanType::Passive,
            EpicsValue::Double(0.0),
            std::any::type_name::<dyn DeviceSupport>(),
            std::any::type_name::<DeviceReadOutcome>(),
        );

        // The helper consumes and returns the builder — chain on .build().
        let _builder = register_asyn_device_support_for_builder(IocBuilder::new());
    }

    #[test]
    fn test_parse_full() {
        let link = parse_asyn_link("@asyn(myPort, 0, 1.0) TEMPERATURE").unwrap();
        assert_eq!(link.port_name, "myPort");
        assert_eq!(link.addr, 0);
        assert_eq!(link.timeout, Some(Duration::from_secs_f64(1.0)));
        assert_eq!(link.drv_info, "TEMPERATURE");
    }

    #[test]
    fn test_parse_port_only() {
        let link = parse_asyn_link("@asyn(port1) PARAM").unwrap();
        assert_eq!(link.port_name, "port1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.timeout, Some(Duration::from_secs(1)));
        assert_eq!(link.drv_info, "PARAM");
    }

    #[test]
    fn test_parse_port_and_addr() {
        let link = parse_asyn_link("@asyn(port2, 3) VALUE").unwrap();
        assert_eq!(link.port_name, "port2");
        assert_eq!(link.addr, 3);
        assert_eq!(link.drv_info, "VALUE");
    }

    #[test]
    fn test_parse_fractional_timeout() {
        let link = parse_asyn_link("@asyn(dev, 1, 0.5) CMD").unwrap();
        assert_eq!(link.timeout, Some(Duration::from_secs_f64(0.5)));
    }

    #[test]
    fn test_parse_no_drv_info() {
        let link = parse_asyn_link("@asyn(port1)").unwrap();
        assert_eq!(link.drv_info, "");
    }

    #[test]
    fn test_parse_invalid_prefix() {
        assert!(parse_asyn_link("@wrong(port)").is_err());
    }

    #[test]
    fn test_parse_missing_paren() {
        assert!(parse_asyn_link("@asyn(port").is_err());
    }

    #[test]
    fn test_parse_invalid_addr() {
        assert!(parse_asyn_link("@asyn(port, abc) X").is_err());
    }

    #[test]
    fn test_parse_invalid_timeout() {
        assert!(parse_asyn_link("@asyn(port, 0, xyz) X").is_err());
    }

    /// R9-54: C `strtod` accepts the negative "wait forever" sentinel
    /// (asynEpicsUtils.c:125) and every other double the operator can type.
    /// `Duration::from_secs_f64` panicked on exactly those, aborting the thread
    /// at record init. The sentinel is now carried, not substituted.
    #[test]
    fn test_parse_negative_timeout_waits_forever_and_non_finite_is_bounded() {
        // A negative link timeout is C's "wait forever" and reaches the driver
        // as one (DRV-42); it used to take the bounded default here.
        for spec in ["@asyn(PORT, 0, -1) DRV", "@asyn(PORT, 0, -0.5) DRV"] {
            let link = parse_asyn_link(spec).expect(spec);
            assert_eq!(link.timeout, None, "{spec}");
            assert_eq!(link.drv_info, "DRV", "{spec}");
        }
        // The values C itself has no defined behaviour for keep the bounded
        // fallback.
        for spec in [
            "@asyn(PORT, 0, inf) DRV",
            "@asyn(PORT, 0, NaN) DRV",
            "@asyn(PORT, 0, 1e30) DRV",
        ] {
            let link = parse_asyn_link(spec).expect(spec);
            assert_eq!(link.timeout, Some(DEFAULT_TIMEOUT), "{spec}");
            assert_eq!(link.drv_info, "DRV", "{spec}");
        }
        // A representable timeout still passes through verbatim.
        assert_eq!(
            parse_asyn_link("@asyn(PORT, 0, 0) DRV").unwrap().timeout,
            Some(Duration::ZERO)
        );
    }

    /// R9-54, same field on the `@asynMask` parser.
    #[test]
    fn test_parse_mask_negative_timeout_waits_forever() {
        let link = parse_asyn_mask_link("@asynMask(PORT, 0, 0x1F, -1) DRV").unwrap();
        assert_eq!(
            link.timeout, None,
            "a negative mask-link timeout waits forever"
        );
        assert_eq!(link.mask, 0x1F);
        assert_eq!(link.drv_info, "DRV");
    }

    #[test]
    fn test_parse_space_separated() {
        // NDCircularBuff.template uses space-separated format: @asyn(PORT 0)DRVINFO
        let link = parse_asyn_link("@asyn(CB1 0)CIRC_BUFF_CONTROL").unwrap();
        assert_eq!(link.port_name, "CB1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.drv_info, "CIRC_BUFF_CONTROL");
    }

    #[test]
    fn test_parse_space_separated_with_timeout() {
        let link = parse_asyn_link("@asyn(PORT1 2 1.5) PARAM").unwrap();
        assert_eq!(link.port_name, "PORT1");
        assert_eq!(link.addr, 2);
        assert_eq!(link.timeout, Some(Duration::from_secs_f64(1.5)));
        assert_eq!(link.drv_info, "PARAM");
    }

    #[test]
    fn test_parse_addr_hex_base0() {
        // C asynEpicsUtils.c:114 strtol(.., 0): `0x10` is hex 16.
        let link = parse_asyn_link("@asyn(port, 0x10) PARAM").unwrap();
        assert_eq!(link.addr, 16);
        let upper = parse_asyn_link("@asyn(port, 0X1F) PARAM").unwrap();
        assert_eq!(upper.addr, 31);
    }

    #[test]
    fn test_parse_addr_octal_base0() {
        // strtol(.., 0): a leading `0` selects octal, so `010` is 8 — NOT
        // decimal 10, which the old decimal-only parser silently bound.
        let link = parse_asyn_link("@asyn(port, 010) PARAM").unwrap();
        assert_eq!(link.addr, 8);
    }

    #[test]
    fn test_parse_addr_decimal_and_zero_unchanged() {
        // Plain decimal and a bare "0" still parse as base 10.
        assert_eq!(parse_asyn_link("@asyn(p, 10) X").unwrap().addr, 10);
        assert_eq!(parse_asyn_link("@asyn(p, 0) X").unwrap().addr, 0);
        assert_eq!(parse_asyn_link("@asyn(p, -5) X").unwrap().addr, -5);
    }

    // --- asynMask link tests ---

    #[test]
    fn test_parse_mask_link_full() {
        let link = parse_asyn_mask_link("@asynMask(port1, 0, 0xFF, 2.0) BITS").unwrap();
        assert_eq!(link.port_name, "port1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.mask, 0xFF);
        assert_eq!(link.timeout, Some(Duration::from_secs_f64(2.0)));
        assert_eq!(link.drv_info, "BITS");
    }

    #[test]
    fn test_parse_mask_link_no_timeout() {
        let link = parse_asyn_mask_link("@asynMask(port1, 0, 255) BITS").unwrap();
        assert_eq!(link.mask, 255);
        assert_eq!(link.timeout, Some(Duration::from_secs(1)));
    }

    #[test]
    fn test_parse_mask_link_hex_upper() {
        let link = parse_asyn_mask_link("@asynMask(p, 0, 0XFF00) X").unwrap();
        assert_eq!(link.mask, 0xFF00);
    }

    #[test]
    fn test_parse_mask_link_too_few_args() {
        assert!(parse_asyn_mask_link("@asynMask(port1, 0) BITS").is_err());
    }

    #[test]
    fn test_parse_mask_link_invalid_prefix() {
        assert!(parse_asyn_mask_link("@asyn(port1, 0, 0xFF) BITS").is_err());
    }

    #[test]
    fn test_parse_mask_link_negative_nbits_binds() {
        // asynInt32 @asynMask 3rd arg is a signed bit count; a negative
        // count (bipolar) must bind, not fail the u32 parse. Stored as the
        // 32-bit pattern: -8 => 0xFFFFFFF8, recoverable via `as i32`.
        let link = parse_asyn_mask_link("@asynMask(p, 0, -8) X").unwrap();
        assert_eq!(link.mask as i32, -8);
    }

    #[test]
    fn test_parse_mask_link_octal_base0() {
        // C asynEpicsUtils.c:193 strtoul(.., 0): mask `010` is octal 8.
        let link = parse_asyn_mask_link("@asynMask(p, 0, 010) X").unwrap();
        assert_eq!(link.mask, 8);
    }

    #[test]
    fn test_parse_mask_link_addr_hex_base0() {
        // The mask-link addr is also strtol(.., 0) (asynEpicsUtils.c:186).
        let link = parse_asyn_mask_link("@asynMask(p, 0x2, 0xFF) X").unwrap();
        assert_eq!(link.addr, 2);
        assert_eq!(link.mask, 0xFF);
    }

    /// C `parseLink` / `parseLinkMask` (asynEpicsUtils.c:91-213), one case
    /// per boundary of their scan: `(link, mask?, port, addr, mask, timeout
    /// secs (None = forever), drvInfo)` or the C errorMessage.
    #[test]
    fn link_parsers_take_what_c_takes_and_refuse_what_it_refuses() {
        type Want = Result<(&'static str, i32, u32, Option<f64>, &'static str), &'static str>;
        let plain_err = Err(PARSE_LINK_ERROR);
        let mask_err = Err(PARSE_LINK_MASK_ERROR);
        let cases: &[(&str, bool, Want)] = &[
            // separators: comma, whitespace, and both mixed
            ("@asyn(p,1,2.5)X", false, Ok(("p", 1, 0, Some(2.5), "X"))),
            ("@asyn(p 1 2.5) X", false, Ok(("p", 1, 0, Some(2.5), "X"))),
            ("@asyn( p , 1 2.5)X", false, Ok(("p", 1, 0, Some(2.5), "X"))),
            ("@asyn(p ,, 1,)X", false, Ok(("p", 1, 0, Some(1.0), "X"))),
            // the timeout closes on whitespace only (`skipWhite(pnext, 0)`)
            ("@asyn(p 1 2.5,)X", false, plain_err),
            ("@asyn(p 1 2.5 3)X", false, plain_err),
            // optional fields and the drvInfo after `)`
            ("@asyn(p)", false, Ok(("p", 0, 0, Some(1.0), ""))),
            ("@asyn(p 0)  a b", false, Ok(("p", 0, 0, Some(1.0), "a b"))),
            ("@asyn(p,0,-1)X", false, Ok(("p", 0, 0, None, "X"))),
            ("@asyn(,1)X", false, Ok(("", 1, 0, Some(1.0), "X"))),
            // refusals
            ("@asyn(p", false, plain_err),
            ("@asyn(p 1", false, plain_err),
            ("@asyn(p x)", false, plain_err),
            ("@asyn(p 1 1e999)", false, plain_err),
            ("@asynMask(p 0 1)", false, plain_err),
            ("p 0", false, Err("Link must be INST_IO or VME_IO")),
            // asynMask: addr and mask required
            (
                "@asynMask(SP1_Bit_In 0 0x1)",
                true,
                Ok(("SP1_Bit_In", 0, 1, Some(1.0), "")),
            ),
            (
                "@asynMask(SP2 20 10)MODBUS_DATA",
                true,
                Ok(("SP2", 20, 10, Some(1.0), "MODBUS_DATA")),
            ),
            (
                "@asynMask(p,0,0xFF,2)D",
                true,
                Ok(("p", 0, 0xFF, Some(2.0), "D")),
            ),
            (
                "@asynMask(p, 0 -8)D",
                true,
                Ok(("p", 0, 0xFFFF_FFF8, Some(1.0), "D")),
            ),
            ("@asynMask(p)", true, mask_err),
            ("@asynMask(p 0)", true, mask_err),
            ("@asynMask(p 0 0x1q)", true, mask_err),
            ("@asynMask(p 0 zz)", true, mask_err),
            ("@asyn(p 0 1)", true, mask_err),
            ("p 0 1", true, Err("Link must be INST_IO")),
        ];
        for &(link, mask, want) in cases {
            let got = parse_inst_io(link, mask).map(|io| {
                (
                    io.port_name,
                    io.addr,
                    io.mask,
                    io.timeout.map(|t| t.as_secs_f64()),
                    io.drv_info,
                )
            });
            let want = want.map(|(p, a, m, t, d)| (p.to_string(), a, m, t, d.to_string()));
            assert_eq!(got, want, "{link}");
        }
    }

    /// Each DTYP parses with its C dset's parser, and a link that parser
    /// refuses binds a device whose init prints that dset's line and takes the
    /// `bad:` arm, instead of the record having no device support.
    #[test]
    fn a_refused_link_prints_its_dsets_line_and_kills_the_record() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        let cases = [
            (
                "ai",
                "asynInt32",
                "@asyn(p 0 1 2)",
                Some("R devAsynInt32::initCommon  "),
                PARSE_LINK_MASK_ERROR,
            ),
            (
                "busy",
                "asynInt32",
                "@asyn(p 0 1 2)",
                Some("R devBusyAsyn::initCommon  "),
                PARSE_LINK_MASK_ERROR,
            ),
            (
                "bi",
                "asynUInt32Digital",
                "@asyn(p 0)",
                Some("R devAsynUInt32Digital::initCommon "),
                PARSE_LINK_MASK_ERROR,
            ),
            (
                "ai",
                "asynFloat64",
                "@asynMask(p 0 1)",
                Some("R devAsynFloat64::initCommon "),
                PARSE_LINK_ERROR,
            ),
            (
                "ai",
                "asynInt64",
                "@asyn(p x)",
                Some("R devAsynInt64::initCommon  "),
                PARSE_LINK_ERROR,
            ),
            (
                "stringin",
                "asynOctetRead",
                "",
                Some("R devAsynOctet::initCommon error in link "),
                "Link must be INST_IO or VME_IO",
            ),
            (
                "waveform",
                "asynInt8ArrayIn",
                "@asyn(p",
                Some("devAsynXXXArray::devAsynXXXArray, R error in link "),
                PARSE_LINK_ERROR,
            ),
            (
                "waveform",
                "asynInt32TimeSeries",
                "@asyn(p",
                None,
                PARSE_LINK_ERROR,
            ),
        ];
        for (record_type, dtyp, inp, prefix, message) in cases {
            let ctx = DeviceSupportContext {
                record_type,
                dtyp,
                inp,
                out: "",
            };
            let dset = Dset::of(record_type, dtyp);
            assert_eq!(dset.parse(inp).err(), Some(message), "{dtyp} {inp}");
            let (line, errlog) = dset.refusal_line("R", &LinkRefusal::Parse(message));
            let prefix = prefix.unwrap_or("devAsynInt32TimeSeries::initCommon, R error in link ");
            assert_eq!(line, format!("{prefix}{message}"), "{dtyp}");
            assert_eq!(errlog, matches!(dset, Dset::Array | Dset::TimeSeries(_)));

            let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
            assert_eq!(dev.dtyp(), dtyp);
            dev.set_record_info("R", ScanType::Passive);
            let mut rec = epics_base_rs::server::records::ai::AiRecord::default();
            let outcome = dev.init(&mut rec).unwrap();
            let want = if dtyp.ends_with("TimeSeries") {
                DeviceInitOutcome::dead()
            } else {
                DeviceInitOutcome::dead_with_alarm(
                    epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                    epics_base_rs::server::record::AlarmSeverity::Invalid,
                )
            };
            assert_eq!(outcome, want, "{dtyp}");
        }
        // The forms each C dset accepts: asynInt32 falls back to the mask
        // parser and keeps its mask, a plain link leaves the mask off.
        let int32 = Dset::of("ai", "asynInt32");
        assert_eq!(int32.parse("@asynMask(p 0 8)D").unwrap().1, Some(8));
        assert_eq!(int32.parse("@asyn(p 0)D").unwrap().1, None);
    }

    /// A drvInfo the port has no parameter for: each dset prints its own
    /// `drvUserCreate` line, where C does (devAsynInt32.c:272-276,
    /// devAsynOctet.c:615-619), behind asynPortDriver's trace line. The base
    /// class leaves `errorMessage` empty. A child, to read its stdout.
    #[test]
    fn a_missing_parameter_prints_the_dsets_drv_user_create_line() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;

        const CHILD: &str = "ASYN_DRV_USER_LINE_CHILD";
        const TEST: &str =
            "adapter::tests::a_missing_parameter_prints_the_dsets_drv_user_create_line";
        let cases = [
            (
                "ai",
                "asynInt32",
                "DU:I32 devAsynInt32::initCommon drvUserCreate ",
            ),
            (
                "ai",
                "asynFloat64",
                "DU:F64 devAsynFloat64::initCommon drvUserCreate ",
            ),
            (
                "stringin",
                "asynOctetRead",
                "DU:OCT devAsynOctet::initDrvUser drvUserCreate failed ",
            ),
            // devAsynOctet.c:631-637: a command-response with no command.
            (
                "stringin",
                "asynOctetCmdResponse",
                "DU:CMD  devAsynOctet::initCmdBuffer no userParam",
            ),
        ];
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{stdout}{stderr}");
            for (_, _, line) in cases {
                assert!(stdout.lines().any(|l| l == line), "{line:?}: {stdout}");
            }
            assert!(
                !stdout.contains("[asyn]") && !stderr.contains("[asyn]"),
                "{stderr}"
            );
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestPort::new()), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            "du_port".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::asyn_record::register_port("du_port", handle, &crate::registry::open_owner())
            .unwrap();
        for (record_type, dtyp, line) in cases {
            let cmd = dtyp == "asynOctetCmdResponse";
            let ctx = DeviceSupportContext {
                record_type,
                dtyp,
                inp: if cmd {
                    "@asyn(du_port,0)"
                } else {
                    "@asyn(du_port,0)NOPE"
                },
                out: "",
            };
            let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
            dev.set_record_info(line.split(' ').next().unwrap(), ScanType::Passive);
            let mut rec = epics_base_rs::server::records::ai::AiRecord::default();
            assert_eq!(
                dev.init(&mut rec).unwrap(),
                DeviceInitOutcome::dead_with_alarm(
                    epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                    epics_base_rs::server::record::AlarmSeverity::Invalid,
                ),
                "{dtyp}"
            );
        }
    }

    /// Each dset's `drvUserCreate` refusal: devAsynInt32.c:273,
    /// devBusyAsyn.c:193, devAsynUInt32Digital.c:228, devAsynFloat64.c:187,
    /// devAsynInt64.c:229 and devAsynOctet.c:616 printf it;
    /// devAsynXXXArray.cpp:153 and devAsynXXXTimeSeries.h:102 errlog it.
    #[test]
    fn a_drv_user_refusal_is_each_dsets_line() {
        let cases = [
            (
                "ai",
                "asynInt32",
                "R devAsynInt32::initCommon drvUserCreate m",
            ),
            (
                "busy",
                "asynInt32",
                "R devBusyAsyn::initCommon drvUserCreate m",
            ),
            (
                "bi",
                "asynUInt32Digital",
                "R devAsynUInt32Digital::initCommon drvUserCreate m",
            ),
            (
                "ai",
                "asynFloat64",
                "R devAsynFloat64::initCommon drvUserCreate m",
            ),
            (
                "int64in",
                "asynInt64",
                "R devAsynInt64::initCommon drvUserCreate m",
            ),
            (
                "stringin",
                "asynOctetRead",
                "R devAsynOctet::initDrvUser drvUserCreate failed m",
            ),
            (
                "waveform",
                "asynInt8ArrayIn",
                "devAsynXXXArray::devAsynXXXArray, R drvUserCreate failed m",
            ),
            (
                "waveform",
                "asynFloat64TimeSeries",
                "devAsynFloat64TimeSeries::initCommon, R drvUserCreate failed m",
            ),
        ];
        for (record_type, dtyp, want) in cases {
            let dset = Dset::of(record_type, dtyp);
            let (line, errlog) = dset.refusal_line("R", &LinkRefusal::DrvUser("m".into()));
            assert_eq!(line, want, "{dtyp}");
            assert_eq!(errlog, matches!(dset, Dset::Array | Dset::TimeSeries(_)));
        }
        assert_eq!(drv_user_message(&AsynError::ParamNotFound("X".into())), "");
    }

    /// devAsynXXXTimeSeries.h:118-120: the `bad:` arm a failed `drvUserCreate`
    /// takes raises no alarm.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_time_series_with_a_missing_parameter_dies_without_an_alarm() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestPort::new()), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            "du_ts".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::asyn_record::register_port("du_ts", handle, &crate::registry::open_owner()).unwrap();
        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynInt32TimeSeries",
            inp: "@asyn(du_ts,0)NOPE",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
        dev.set_record_info("DU:TS", ScanType::Passive);
        let mut rec = WaveformRecord::new(4, DbFieldType::Long);
        assert_eq!(dev.init(&mut rec).unwrap(), DeviceInitOutcome::dead());
    }

    /// devAsynXXXTimeSeries.h:73-77: an FTVL that is not the interface's
    /// type errlogs C's line and dies without an alarm.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_time_series_with_the_wrong_ftvl_errlogs_cs_line() {
        use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestPort::new()), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            "du_ftvl".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::asyn_record::register_port("du_ftvl", handle, &crate::registry::open_owner())
            .unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        errlog_add_listener(move |m: &str| sink.lock().unwrap().push(m.to_string()));

        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynInt32TimeSeries",
            inp: "@asyn(du_ftvl,0)VAL",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
        dev.set_record_info("DU:FTVL", ScanType::Passive);
        let mut rec = WaveformRecord::new(4, DbFieldType::Double);
        assert_eq!(dev.init(&mut rec).unwrap(), DeviceInitOutcome::dead());
        errlog_flush();
        let line = "devAsynInt32TimeSeries::initCommon, DU:FTVL field type must be \
                    SIGNED_TYPE or UNSIGNED_TYPE";
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|m| m.trim_end() == line), "{seen:?}");
    }

    /// devAsynXXXTimeSeries.h:73-77 checks FTVL before it parses the link,
    /// connects or calls `drvUserCreate`: a bad FTVL prints only its line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_time_series_checks_ftvl_before_the_link() {
        use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestPort::new()), rx);
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            "du_order".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::asyn_record::register_port("du_order", handle, &crate::registry::open_owner())
            .unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        errlog_add_listener(move |m: &str| sink.lock().unwrap().push(m.to_string()));

        let cases = [
            ("DU:ORD_PARAM", "@asyn(du_order,0)NOPE"),
            ("DU:ORD_PORT", "@asyn(NOPORT_ORDER,0)VAL"),
        ];
        for (name, inp) in cases {
            let ctx = DeviceSupportContext {
                record_type: "waveform",
                dtyp: "asynFloat64TimeSeries",
                inp,
                out: "",
            };
            let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
            dev.set_record_info(name, ScanType::Passive);
            let mut rec = WaveformRecord::new(4, DbFieldType::Long);
            assert_eq!(dev.init(&mut rec).unwrap(), DeviceInitOutcome::dead());
        }
        errlog_flush();
        let seen = seen.lock().unwrap();
        for (name, _) in cases {
            let ours: Vec<_> = seen.iter().filter(|m| m.contains(name)).collect();
            let line = format!(
                "devAsynFloat64TimeSeries::initCommon, {name} field type must be \
                 SIGNED_TYPE or UNSIGNED_TYPE"
            );
            assert_eq!(
                ours.iter().map(|m| m.trim_end()).collect::<Vec<_>>(),
                [line.as_str()],
                "{seen:?}"
            );
        }
    }

    /// A link naming a port no one created: the dset still binds, as C's
    /// does, and its init prints C's `connectDevice failed` line and dies.
    #[test]
    fn a_link_to_a_missing_port_prints_connect_device_failed_and_kills_the_record() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;

        let no_port = "asynManager:connectDevice port NOPORT_X not found";
        let cases = [
            (
                "ai",
                "asynInt32",
                "@asyn(NOPORT_X 0)D",
                "R devAsynInt32::initCommon",
            ),
            (
                "busy",
                "asynInt32",
                "@asyn(NOPORT_X 0)D",
                "R devBusyAsyn::initCommon",
            ),
            (
                "bi",
                "asynUInt32Digital",
                "@asynMask(NOPORT_X 0 1)D",
                "R devAsynUInt32Digital::initCommon",
            ),
            (
                "ai",
                "asynFloat64",
                "@asyn(NOPORT_X 0)D",
                "R devAsynFloat64::initCommon",
            ),
            (
                "int64in",
                "asynInt64",
                "@asyn(NOPORT_X 0)D",
                "R devAsynInt64::initCommon",
            ),
            (
                "stringin",
                "asynOctetRead",
                "@asyn(NOPORT_X 0)D",
                "R devAsynOctet::initCommon",
            ),
            (
                "waveform",
                "asynInt8ArrayIn",
                "@asyn(NOPORT_X 0)D",
                "devAsynXXXArray::devAsynXXXArray, R",
            ),
            (
                "waveform",
                "asynInt32TimeSeries",
                "@asyn(NOPORT_X 0)D",
                "devAsynInt32TimeSeries::initCommon, R",
            ),
        ];
        for (record_type, dtyp, inp, prefix) in cases {
            let dset = Dset::of(record_type, dtyp);
            let (line, errlog) = dset.refusal_line("R", &LinkRefusal::NoPort("NOPORT_X".into()));
            assert_eq!(
                line,
                format!("{prefix} connectDevice failed {no_port}"),
                "{dtyp}"
            );
            assert_eq!(errlog, matches!(dset, Dset::Array | Dset::TimeSeries(_)));

            let ctx = DeviceSupportContext {
                record_type,
                dtyp,
                inp,
                out: "",
            };
            let mut dev = universal_asyn_factory(&ctx).expect("the dset binds");
            dev.set_record_info("R", ScanType::Passive);
            let mut rec = epics_base_rs::server::records::ai::AiRecord::default();
            let outcome = dev.init(&mut rec).unwrap();
            let want = if dtyp.ends_with("TimeSeries") {
                DeviceInitOutcome::dead()
            } else {
                DeviceInitOutcome::dead_with_alarm(
                    epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                    epics_base_rs::server::record::AlarmSeverity::Invalid,
                )
            };
            assert_eq!(outcome, want, "{dtyp}");
        }
    }

    #[test]
    fn strtol_reads_hex_octal_decimal_sign_and_stops_where_c_does() {
        assert_eq!(strtol_i32("0x10)"), Some((16, ")")));
        assert_eq!(strtol_i32("010"), Some((8, "")));
        assert_eq!(strtol_i32("10 1.0"), Some((10, " 1.0")));
        assert_eq!(strtol_i32("0"), Some((0, "")));
        assert_eq!(strtol_i32("-0x10"), Some((-16, "")));
        assert_eq!(strtol_i32("+7"), Some((7, "")));
        // C stops at the first character that is not a digit of the base.
        assert_eq!(strtol_i32("08"), Some((0, "8")));
        assert_eq!(strtol_i32("0o20"), Some((0, "o20")));
        assert_eq!(strtol_i32("0x"), Some((0, "x")));
        // No digits: nothing read, `endp == s`.
        assert_eq!(strtol_i32("abc"), Some((0, "abc")));
        assert_eq!(strtol_i32("-x"), Some((0, "-x")));
        // `long` overflow is ERANGE; the `int` assignment then truncates
        // what a 64-bit `long` holds.
        assert_eq!(strtol_i32("99999999999999999999"), None);
        assert_eq!(
            strtol_i32("0x100000001"),
            (std::ffi::c_long::BITS == 64).then_some((1, ""))
        );
    }

    /// glibc's base-0 `strtol`/`strtoul` read C23 `0b` under EPICS'
    /// `_GNU_SOURCE`, so C `parseLinkMask` takes addr 3 and mask 5 here.
    #[test]
    fn a_link_addr_and_mask_take_glibc_s_binary_prefix() {
        let link = parse_asyn_mask_link("@asynMask(p 0b11 0b101 1.0)x").unwrap();
        assert_eq!((link.addr, link.mask), (3, 5));
    }

    #[test]
    fn strtoul_negation_wraps_before_the_u32_cast() {
        // C strtoul negates-then-casts: -8 => 0xFFFFFFF8.
        assert_eq!(strtoul_u32("-8"), Some((0xFFFF_FFF8, "")));
        assert_eq!(strtoul_u32("0xFF00"), Some((0xFF00, "")));
        assert_eq!(strtoul_u32("010"), Some((8, "")));
        assert_eq!(strtoul_u32("255)X"), Some((255, ")X")));
    }

    #[test]
    fn int32_mask_unipolar_from_nbits() {
        // C devAsynInt32.c:239-246 unipolar (positive nbits): mask = low
        // nbits, deviceLow=0, deviceHigh=mask, no sign extension.
        let m = Int32Mask::from_nbits(8).unwrap();
        assert!(!m.bipolar);
        assert_eq!(m.mask, 0xFF);
        assert_eq!(m.sign_bit, 0x80);
        assert_eq!(m.device_low, 0);
        assert_eq!(m.device_high, 255);
        // Masks the low byte; the high "sign" bit is NOT extended.
        assert_eq!(m.apply(0xFF), 255);
        assert_eq!(m.apply(0x80), 128);
        assert_eq!(m.apply(0x1_2345), 0x45);
    }

    #[test]
    fn int32_mask_bipolar_from_nbits() {
        // C devAsynInt32.c:235-243 bipolar (negative nbits): sign-extend on
        // read; deviceLow=~(mask/2)+1, deviceHigh=mask/2.
        let m = Int32Mask::from_nbits(-8).unwrap();
        assert!(m.bipolar);
        assert_eq!(m.mask, 0xFF);
        assert_eq!(m.sign_bit, 0x80);
        assert_eq!(m.device_low, -127);
        assert_eq!(m.device_high, 127);
        // Sign bit set => extend to a negative i32.
        assert_eq!(m.apply(0xFF), -1);
        assert_eq!(m.apply(0x80), -128);
        // Sign bit clear => positive, masked to the low byte.
        assert_eq!(m.apply(0x7F), 127);
        assert_eq!(m.apply(0x1_2345), 0x45);
    }

    #[test]
    fn int32_mask_zero_nbits_is_none() {
        // C leaves mask=0 when no bit count is given => masking is a no-op.
        assert!(Int32Mask::from_nbits(0).is_none());
    }

    #[test]
    fn int32_mask_only_for_asyn_int32_interface() {
        // with_mask derives the nbits config only for the asynInt32
        // interface; a UInt32Digital adapter keeps the raw mask, no nbits.
        // No submit happens here, so the channels need no actor.
        let mk_handle = |name: &str| {
            let interrupts = Arc::new(InterruptManager::new());
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            (
                PortHandle::new(
                    tx,
                    name.into(),
                    interrupts,
                    ActorLink::unowned(),
                    std::sync::Arc::new(crate::trace::TraceManager::new()),
                ),
                _rx,
            )
        };
        let link = |port: &str| AsynLink {
            port_name: port.into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };

        let (h1, _rx1) = mk_handle("p1");
        let int32 =
            AsynDeviceSupport::from_handle(h1, link("p1"), "asynInt32").with_mask((-8i32) as u32);
        assert_eq!(int32.int32_mask, Int32Mask::from_nbits(-8));

        let (h2, _rx2) = mk_handle("p2");
        let uint =
            AsynDeviceSupport::from_handle(h2, link("p2"), "asynUInt32Digital").with_mask(0xFF00);
        assert!(uint.int32_mask.is_none());
        assert_eq!(uint.mask, 0xFF00);
    }

    #[test]
    fn a_polled_read_masks_and_sign_extends_int32() {
        // The polled read applies the nbits mask + sign-extend before the
        // value is used (C processCallbackInput, devAsynInt32.c:485-488).
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = PortHandle::new(
            tx,
            "p".into(),
            interrupts,
            ActorLink::unowned(),
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "p".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        // bipolar 8-bit (@asynMask ... -8): raw 0xFF reads back as -1.
        let ads =
            AsynDeviceSupport::from_handle(handle, link, "asynInt32").with_mask((-8i32) as u32);
        let mut result = RequestResult::int32_read(0xFF);
        Int32Mask::apply_to_read(ads.int32_mask, &mut result);
        assert_eq!(ads.result_to_value(&result), Some(EpicsValue::Long(-1)));
    }

    /// `asynInt64` carries the record's i64 whole on both sides. An
    /// int64out VAL is `EpicsValue::Int64`, which `write_op` had no arm for,
    /// so the put completed without a driver write; the reads went through
    /// `f64` and lost every bit past 2^53. C hands the `epicsInt64` through
    /// (`processLLo` `result.value = pr->val`, `processLLi` `pr->val =
    /// result.value`, devAsynInt64.c:710,668) and casts at the record: ai
    /// `(epicsFloat64)`, longin `(epicsInt32)` (:779,970).
    #[test]
    fn asynint64_carries_the_i64_whole() {
        use crate::param::ParamValue;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::int64in::Int64inRecord;
        use epics_base_rs::server::records::longin::LonginRecord;

        let interrupts = Arc::new(InterruptManager::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = PortHandle::new(
            tx,
            "p".into(),
            interrupts,
            ActorLink::unowned(),
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "p".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "GC_GevSCPD".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt64");

        let v = (1i64 << 53) + 1;
        match ads.write_op(&EpicsValue::Int64(v)) {
            Some(RequestOp::Int64Write { value }) => assert_eq!(value, v),
            other => panic!("an int64out VAL must reach the driver as Int64Write, got {other:?}"),
        }
        assert_eq!(
            ads.result_to_value(&RequestResult::int64_read(v)),
            Some(EpicsValue::Int64(v))
        );
        assert_eq!(
            ads.param_value_for_iface(&ParamValue::Int64(v), false)
                .unwrap(),
            Some(EpicsValue::Int64(v))
        );

        let mut int64in = Int64inRecord::new(0);
        assert!(ads.store_read_value(&mut int64in, EpicsValue::Int64(v)));
        assert_eq!(int64in.val(), Some(EpicsValue::Int64(v)));
        let mut ai = AiRecord::new(0.0);
        ads.store_read_value(&mut ai, EpicsValue::Int64(v));
        assert_eq!(ai.val(), Some(EpicsValue::Double(v as f64)));
        let mut longin = LonginRecord::new(0);
        ads.store_read_value(&mut longin, EpicsValue::Int64(2000));
        assert_eq!(longin.val(), Some(EpicsValue::Long(2000)));
    }

    /// The I/O-Intr twin of `result_to_value_masks_and_sign_extends_int32`.
    ///
    /// C applies `@asynMask` in BOTH input paths — `processCallbackInput`
    /// (devAsynInt32.c:485-488) and `interruptCallbackInput` (:537-539) run the
    /// same `value &= mask` + bipolar sign-extend. A mask on one path only is the
    /// same defect class this module's `param_value_for_iface` exists to close:
    /// the polled read and the interrupt read of one parameter deriving the
    /// record's value by different rules.
    #[test]
    fn io_intr_masks_and_sign_extends_int32_like_the_polled_read() {
        use crate::param::ParamValue;

        let interrupts = Arc::new(InterruptManager::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = PortHandle::new(
            tx,
            "p".into(),
            interrupts,
            ActorLink::unowned(),
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "p".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let ads =
            AsynDeviceSupport::from_handle(handle, link, "asynInt32").with_mask((-8i32) as u32);

        // Same raw sample down both paths of the same adapter: they must agree.
        let mut read = RequestResult::int32_read(0xF0);
        Int32Mask::apply_to_read(ads.int32_mask, &mut read);
        let polled = ads.result_to_value(&read);
        let io_intr = ads
            .param_value_for_iface(&ParamValue::Int32(0xF0), false)
            .expect("an Int32 sample is readable through asynInt32");

        assert_eq!(
            polled,
            Some(EpicsValue::Long(-16)),
            "bipolar 8-bit: 0xF0 masks and sign-extends to -16"
        );
        assert_eq!(
            io_intr, polled,
            "the interrupt read must apply the same @asynMask as the polled read \
             (C interruptCallbackInput, devAsynInt32.c:537-539)"
        );
    }

    /// An array parameter already of the interface's element type reaches
    /// the record as the same buffer; only a differing element type is cast.
    #[test]
    fn an_array_of_the_interface_type_shares_the_parameter_buffer() {
        use crate::param::ParamValue;
        let arc: std::sync::Arc<[f64]> = std::sync::Arc::from([1.5f64, 2.5].as_slice());
        let f64arr = ParamValue::Float64Array(std::sync::Arc::clone(&arc));
        match convert_param_array_to_iface("asynFloat64Array", &f64arr) {
            Some(EpicsValue::DoubleArray(a)) => assert!(std::ptr::eq(a.as_ptr(), arc.as_ptr())),
            other => panic!("{other:?}"),
        }
        match convert_param_array_to_iface("asynFloat32Array", &f64arr) {
            Some(EpicsValue::FloatArray(a)) => assert_eq!(a, vec![1.5f32, 2.5]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn array_interrupt_converts_to_record_interface_type() {
        use crate::param::ParamValue;
        // An F64-native array delivered on the I/O Intr path must be converted to
        // each consuming interface's element type (C fires all six array
        // interfaces type-converted, NDPluginStdArrays.cpp:170-197).
        let f64arr = ParamValue::Float64Array(std::sync::Arc::from([1.7f64, 2.9, -3.1].as_slice()));

        assert_eq!(
            convert_param_array_to_iface("asynInt16Array", &f64arr),
            Some(EpicsValue::ShortArray(vec![1, 2, -3].into())),
        );
        assert_eq!(
            convert_param_array_to_iface("asynFloat64Array", &f64arr),
            Some(EpicsValue::DoubleArray(vec![1.7, 2.9, -3.1].into())),
        );

        // Integer narrowing is a truncating C cast, matching the polled ccast
        // path (40000 as i16 wraps to -25536), not a saturating convert.
        let i32arr = ParamValue::Int32Array(std::sync::Arc::from([40000i32].as_slice()));
        assert_eq!(
            convert_param_array_to_iface("asynInt16Array", &i32arr),
            Some(EpicsValue::ShortArray(vec![-25536].into())),
        );

        // asynInt8Array goes through i8 then reinterprets to u8 (300.0 -> i8
        // saturates to 127 -> u8 127), matching result_to_value's CharArray map.
        let over = ParamValue::Float64Array(std::sync::Arc::from([300.0f64].as_slice()));
        assert_eq!(
            convert_param_array_to_iface("asynInt8Array", &over),
            Some(EpicsValue::CharArray(vec![127].into())),
        );

        // Scalar interfaces are not array interfaces: returns None so the caller
        // falls back to the verbatim mapping.
        assert_eq!(convert_param_array_to_iface("asynFloat64", &f64arr), None);
    }

    use crate::error::AsynResult;
    use crate::interrupt::{InterruptManager, InterruptValue};
    use crate::param::ParamType;
    use crate::port::InterfaceMask;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use std::sync::Arc;

    struct TestPort {
        base: PortDriverBase,
    }
    impl TestPort {
        fn new() -> Self {
            let mut base = PortDriverBase::new(
                "test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
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

    /// Port that reports configurable getBounds — used for the ai
    /// LINEAR ESLO/EOFF wiring tests.
    struct BoundedPort {
        base: PortDriverBase,
        low32: i32,
        high32: i32,
    }
    impl BoundedPort {
        fn new(low32: i32, high32: i32) -> Self {
            let mut base = PortDriverBase::new(
                "test_bounds",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self {
                base,
                low32,
                high32,
            }
        }
    }
    impl PortDriver for BoundedPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn get_bounds_int32(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
            Ok((self.low32, self.high32))
        }
    }

    fn make_bounded_adapter(low: i32, high: i32, iface: &str) -> AsynDeviceSupport {
        let driver = BoundedPort::new(low, high);
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-bounds-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test_bounds".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test_bounds".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.set_record_info("TEST:AI", ScanType::Passive);
        ads
    }

    /// C devAsyn queues a record's process-time request at Low for every
    /// interface but asynOctet (devAsynOctet.c:764, Medium), and reads at init
    /// through `queueLockPort` at Low (asynManager.c:1896).
    #[test]
    fn device_support_queues_at_the_c_dsets_priority() {
        for (iface, process) in [
            ("asynInt32", QueuePriority::Low),
            ("asynInt64", QueuePriority::Low),
            ("asynFloat64", QueuePriority::Low),
            ("asynUInt32Digital", QueuePriority::Low),
            ("asynInt32Array", QueuePriority::Low),
            ("asynFloat64Array", QueuePriority::Low),
            ("asynOctet", QueuePriority::Medium),
        ] {
            let ads = make_bounded_adapter(0, 0, iface);
            assert_eq!(ads.process_user().priority, process, "{iface} at process");
            assert_eq!(
                ads.init_user().priority,
                QueuePriority::Low,
                "{iface} at init"
            );
        }
    }

    /// The dset, not the record, applies MASK to a readback: asynInt32 bo
    /// stores the read raw (devAsynInt32.c:1187), asynInt32 mbbi and every
    /// asynUInt32Digital record AND it (:1296, devAsynUInt32Digital.c).
    #[test]
    fn readback_mask_is_the_dsets() {
        use epics_base_rs::server::records::{bo::BoRecord, mbbi::MbbiRecord};

        let mut ads = make_adapter(ScanType::Passive);
        let mut bo = BoRecord::new(0);
        bo.mask = 0x04;
        assert!(ads.store_read_value(&mut bo, EpicsValue::Long(0x02)));
        assert_eq!((bo.rval, bo.val), (0x02, 1), "asynInt32 bo keeps the raw");

        let mut mbbi = MbbiRecord::new(0);
        mbbi.nobt = 4;
        mbbi.mask = 0x0F;
        assert!(ads.store_read_value(&mut mbbi, EpicsValue::Long(0x32)));
        assert_eq!(mbbi.rval, 0x02, "asynInt32 mbbi masks");

        ads.set_iface_type("asynUInt32Digital");
        let mut bo = BoRecord::new(0);
        bo.mask = 0x04;
        assert!(ads.store_read_value(&mut bo, EpicsValue::Long(0x02)));
        assert_eq!((bo.rval, bo.val), (0, 0), "asynUInt32Digital bo masks");
    }

    fn make_adapter(scan: ScanType) -> AsynDeviceSupport {
        make_adapter_on(scan, Arc::new(InterruptManager::new()))
    }

    fn make_adapter_on(scan: ScanType, interrupts: Arc<InterruptManager>) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-adapter-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );

        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
        ads.set_record_info("TEST:REC", scan);
        ads
    }

    /// iocInit's `scanAdd` for a record loaded on I/O Intr: `get_ioint_info(0)`,
    /// then the pump's receiver.
    fn join_io_intr(
        ads: &mut AsynDeviceSupport,
        rec: &mut dyn Record,
    ) -> Option<tokio::sync::mpsc::Receiver<()>> {
        let mut common = epics_base_rs::server::record::CommonFields::default();
        let add = epics_base_rs::server::device_support::IoIntrCmd::Add;
        assert!(ads.get_ioint_info(rec, &mut common, add));
        ads.io_intr_receiver()
    }

    /// C `initCommon` registers the `asyn:READBACK` callback in `init_record`
    /// (devAsynInt32.c:325-341), so its refusal prints there, ahead of the
    /// initial readback; iocInit's `io_intr_receiver` hands on the record's
    /// `ioScanPvt` and registers nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readback_registers_in_init() {
        use epics_base_rs::server::records::bo::BoRecord;
        for (sources, live) in [(InterfaceMask::INT32, true), (InterfaceMask::NONE, false)] {
            let mut ads = make_adapter_on(
                ScanType::Passive,
                Arc::new(InterruptManager::with_sources(sources)),
            );
            ads.set_asyn_readback(true);
            ads.init(&mut BoRecord::new(0)).unwrap();
            assert_eq!(ads.interrupt_sub.is_some(), live, "{sources:?} after init");
            assert!(ads.io_intr_receiver().is_some(), "{sources:?} at iocInit");
            assert_eq!(ads.interrupt_sub.is_some(), live, "{sources:?} at iocInit");
        }
    }

    /// C `initAiAverage` registers the average callback in `init_record`
    /// (devAsynInt32.c:870-872) in every SCAN; iocInit's `io_intr_receiver`
    /// hands on the `ioScanPvt` a later move to I/O Intr wakes, and registers
    /// nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_registers_in_init() {
        use epics_base_rs::server::records::ai::AiRecord;
        for scan in [ScanType::Passive, ScanType::IoIntr] {
            let mut ads = make_average_adapter("asynInt32");
            ads.set_record_info("TEST:AVG", scan);
            ads.init(&mut AiRecord::new(0.0)).unwrap();
            assert!(ads.average_callback_sub.is_some(), "{scan:?} after init");
            assert!(ads.io_intr_receiver().is_some(), "{scan:?} at iocInit");
        }
    }

    /// Build a Passive asynInt32 input adapter whose port has VAL pre-seeded to
    /// `value` with the given device read `status`. Used to exercise the polled
    /// read value-discard gate: a non-success param status must keep the prior
    /// record value (C processAi return -1) while still raising the alarm.
    fn make_seeded_int32_adapter(
        value: i32,
        status: crate::error::AsynStatus,
    ) -> AsynDeviceSupport {
        let mut driver = TestPort::new();
        driver.base.params.set_int32(0, 0, value).unwrap();
        driver.base.params.set_status(0, 0, status).unwrap();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-seeded-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
        ads.set_record_info("TEST:SEED", ScanType::Passive);
        ads.reason_set = true; // reason defaults to 0 = VAL; skip init's drv_user_create
        ads
    }

    /// Build an averaging (`asynInt32Average` / `asynFloat64Average`) adapter:
    /// a plain ai adapter on the base interface with `average` set, on a
    /// periodic (non-IoIntr) SCAN. `init` arms the accumulating interrupt;
    /// the port is never read by the average path.
    fn make_average_adapter(iface: &str) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-average-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.average = Some(Arc::new(AverageState::new()));
        ads.set_record_info("TEST:AVG", ScanType::Passive);
        ads
    }

    /// C `processAiAverage` takes the ring's decimated mean only when there is
    /// one (devAsynInt32.c:895-898); an I/O Intr pass on an empty ring drains
    /// the running mean like a periodic one, and with no samples is UDF.
    #[test]
    fn average_mode1_on_an_empty_ring_drains_the_running_mean() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.set_record_info("TEST:AVG", ScanType::IoIntr);
        let mut rec = AiRecord::new(0.0);
        rec.sval = 4.0;
        ads.init(&mut rec).unwrap();
        let acc = ads.average.clone().unwrap();
        acc.averager.push(10.0);
        acc.averager.push(20.0);

        ads.read(&mut rec).unwrap();
        assert_eq!(rec.rval, 15);
        assert_eq!(acc.averager.count(), 0, "the period is consumed");

        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm_status,
            epics_base_rs::server::recgbl::alarm_status::UDF_ALARM
        );
        assert_eq!(rec.rval, 15, "no samples: the value stands");
    }

    /// Poll the averager until at least `n` samples have accumulated (the
    /// interrupt callback runs on a spawned task). Panics if `n` never arrives.
    async fn await_average_count(acc: &Arc<AverageState>, n: u64) {
        for _ in 0..200 {
            if acc.averager.count() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("averager never reached {n} samples");
    }

    /// Build a time-series adapter on the given (already normalized) base
    /// interface, with `time_series` armed and a Passive record scan — the
    /// shape the factory produces for `asynXxxTimeSeries`.
    fn make_timeseries_adapter(iface: &str) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-ts-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.time_series = Some(Arc::new(TimeSeriesState::new()));
        ads.set_record_info("TEST:TS", ScanType::Passive);
        ads
    }

    /// Poll the time-series buffer until it holds at least `n` samples (the
    /// accumulating callback runs on a spawned task). Panics if `n` never arrives.
    async fn await_ts_len(ts: &Arc<TimeSeriesState>, n: usize) {
        for _ in 0..200 {
            let len = ts
                .inner
                .lock()
                .unwrap()
                .buf
                .as_ref()
                .map(TsBuf::len)
                .unwrap_or(0);
            if len >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("time-series buffer never reached {n} samples");
    }

    /// asynInt32TimeSeries happy path: RARM=1 starts acquisition (BUSY=1,
    /// NORD=0), the callback appends only while BUSY, the buffer fills at NELM
    /// (BUSY auto-clears), and the completion process commits VAL/NORD. C
    /// `devAsynXXXTimeSeries.h` interruptCallback + process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_int32_accumulates_while_armed_then_commits_on_full() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynInt32");
        let mut rec = WaveformRecord::new(4, DbFieldType::Long); // FTVL=LONG
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();

        // Arming returns a reprocess wakeup channel (unlike averaging).
        assert!(
            ads.io_intr_receiver().is_some(),
            "time-series records get a per-callback reprocess channel"
        );

        // A sample before RARM=1 (not BUSY) must be dropped (C `if (pPvt->busy)`).
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(99),
            timestamp: SystemTime::now(),
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            ts.inner.lock().unwrap().buf.as_ref().unwrap().len(),
            0,
            "samples before RARM=1 are not accumulated"
        );

        // caput RARM=1 → start: BUSY=1, NORD=0, RARM reset to 0.
        rec.rarm = 1;
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "VAL written directly → convert skipped"
        );
        assert_eq!(rec.rarm, 0, "process resets RARM to 0");
        assert_eq!(rec.busy, 1, "RARM=1 sets BUSY (acquiring)");
        assert_eq!(rec.nord, 0, "start zeroes NORD");

        // Stream NELM samples → buffer fills, BUSY auto-clears on the last.
        for v in [10, 20, 30, 40] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 4).await;
        assert!(
            !ts.inner.lock().unwrap().busy,
            "buffer full clears BUSY (acquisition complete)"
        );

        // The completion process commits the array.
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute());
        assert_eq!(rec.nord, 4, "NORD = samples accumulated");
        assert_eq!(rec.busy, 0, "BUSY cleared after completion");
        match &rec.val {
            EpicsValue::LongArray(v) => {
                assert_eq!(&v[..4], &[10, 20, 30, 40], "VAL holds the samples")
            }
            other => panic!("VAL should be LongArray, got {other:?}"),
        }
    }

    /// RARM=2 stops acquisition mid-fill: the partial buffer commits (NORD =
    /// samples so far), BUSY clears, and a later RARM=3 resumes appending into
    /// the same buffer. C process RARM 2=stop / 3=resume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_rarm_stop_commits_partial_then_resume_continues() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynInt32");
        let mut rec = WaveformRecord::new(8, DbFieldType::Long);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();
        assert!(ads.io_intr_receiver().is_some());

        // Start, accumulate 2 of 8.
        rec.rarm = 1;
        ads.read(&mut rec).unwrap();
        for v in [11, 22] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 2).await;

        // Stop: BUSY=0, partial commit (NORD=2, VAL[..2] = samples).
        rec.rarm = 2;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.busy, 0, "RARM=2 clears BUSY");
        assert_eq!(rec.nord, 2, "stop commits the partial buffer (NORD=2)");
        match &rec.val {
            EpicsValue::LongArray(v) => assert_eq!(&v[..2], &[11, 22]),
            other => panic!("got {other:?}"),
        }

        // A sample while stopped is dropped.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(99),
            timestamp: SystemTime::now(),
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            ts.inner.lock().unwrap().buf.as_ref().unwrap().len(),
            2,
            "stopped acquisition does not accumulate"
        );

        // Resume: BUSY=1, appends continue into the same buffer.
        rec.rarm = 3;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.busy, 1, "RARM=3 re-arms BUSY");
        for v in [33, 44] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 4).await;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 4, "resume continued the same buffer");
        match &rec.val {
            EpicsValue::LongArray(v) => assert_eq!(&v[..4], &[11, 22, 33, 44]),
            other => panic!("got {other:?}"),
        }
    }

    /// A second RARM=1 after a partial acquisition restarts from empty rather
    /// than appending: NORD returns to 0, the record array is wiped, and the
    /// samples that follow start at index 0. C does the wipe explicitly with
    /// `pPvt->nord = 0` plus `memset(pwf->bptr, 0, pwf->nelm*sizeof(EPICS_TYPE))`
    /// (devAsynXXXTimeSeries.h:139-141); the port clears the accumulator and
    /// wipes the record array with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_rearm_discards_the_previous_acquisition() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynInt32");
        let mut rec = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();
        // Registers the accumulating callback (the port's stand-in for C's
        // registerInterruptUser), so it must run before any sample is notified.
        assert!(ads.io_intr_receiver().is_some());

        // First acquisition: 3 of 4 samples, then stop to commit the partial.
        rec.rarm = 1;
        ads.read(&mut rec).unwrap();
        for v in [11, 22, 33] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 3).await;
        rec.rarm = 2;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 3, "first acquisition committed 3 samples");

        // Re-arm. C zeroes the array and resets nord before anything else in
        // the switch, so the commit that follows in the SAME process sees an
        // empty buffer.
        rec.rarm = 1;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.busy, 1, "re-arm sets BUSY");
        assert_eq!(rec.nord, 0, "re-arm zeroes NORD in the arming process");
        match &rec.val {
            EpicsValue::LongArray(v) => assert_eq!(
                &v[..3],
                &[0, 0, 0],
                "re-arm wipes the previous samples (C memset), got {v:?}"
            ),
            other => panic!("VAL should be LongArray, got {other:?}"),
        }

        // The new samples start at index 0 — they do not extend the old run.
        for v in [55, 66] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 2).await;
        rec.rarm = 2;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 2, "NORD counts only the re-armed samples");
        match &rec.val {
            EpicsValue::LongArray(v) => {
                assert_eq!(&v[..2], &[55, 66], "the pre-re-arm samples are gone")
            }
            other => panic!("VAL should be LongArray, got {other:?}"),
        }
    }
    /// asynFloat64TimeSeries commits a DOUBLE array (FTVL=DOUBLE).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_float64_commits_double_array() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynFloat64");
        let mut rec = WaveformRecord::new(3, DbFieldType::Double);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();
        assert!(ads.io_intr_receiver().is_some());

        rec.rarm = 1;
        ads.read(&mut rec).unwrap();
        for v in [1.5, 2.5, 3.5] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Float64(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 3).await;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 3);
        assert_eq!(rec.busy, 0, "BUSY clears when the buffer fills");
        match &rec.val {
            EpicsValue::DoubleArray(v) => assert_eq!(&v[..3], &[1.5, 2.5, 3.5]),
            other => panic!("VAL should be DoubleArray, got {other:?}"),
        }
    }

    /// asynInt64TimeSeries commits an INT64 array (FTVL=INT64).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_int64_commits_int64_array() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynInt64");
        let mut rec = WaveformRecord::new(2, DbFieldType::Int64);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();
        assert!(ads.io_intr_receiver().is_some());

        rec.rarm = 1;
        ads.read(&mut rec).unwrap();
        for v in [9_000_000_000_i64, -9_000_000_000_i64] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int64(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_ts_len(&ts, 2).await;
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 2);
        match &rec.val {
            EpicsValue::Int64Array(v) => {
                assert_eq!(&v[..2], &[9_000_000_000_i64, -9_000_000_000_i64])
            }
            other => panic!("VAL should be Int64Array, got {other:?}"),
        }
    }

    /// FTVL not the signed/unsigned EPICS type of the interface → C's
    /// `initRecord` prints and takes `goto bad`, whose body is `pr->pact = 1;
    /// return -1` (devAsynXXXTimeSeries.h:73-77, 118-120). The record is DEAD:
    /// `dbProcess` refuses every entry, so it never acquires whatever a client
    /// puts to RARM.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_invalid_ftvl_kills_the_record() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        // asynInt32 requires FTVL LONG/ULONG; DOUBLE is invalid.
        let mut ads = make_timeseries_adapter("asynInt32");
        let mut rec = WaveformRecord::new(4, DbFieldType::Double);
        assert_eq!(
            ads.init(&mut rec).unwrap(),
            DeviceInitOutcome::dead(),
            "C `goto bad` — the framework must leave this record in PACT"
        );
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let ts = ads.time_series.clone().unwrap();
        assert!(
            ts.inner.lock().unwrap().buf.is_none(),
            "the dead arm returns before the buffer is sized"
        );

        // The interrupt callback runs whatever the record's state — C's
        // `interruptCallback` is registered by process, which a dead record
        // never reaches, so nothing accumulates.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(7),
            timestamp: SystemTime::now(),
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            ts.inner.lock().unwrap().buf.is_none(),
            "no buffer → nothing accumulates"
        );
        assert_eq!(rec.nord, 0, "NORD untouched when the record cannot acquire");
        assert_eq!(rec.busy, 0, "BUSY stays 0");
    }

    /// The valid-FTVL twin, so the kill is not a blanket refusal: a LONG-FTVL
    /// waveform on asynInt32 comes back Live and gets its typed buffer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_valid_ftvl_leaves_the_record_live() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = make_timeseries_adapter("asynInt32");
        let mut rec = WaveformRecord::new(4, DbFieldType::Long);
        assert_eq!(ads.init(&mut rec).unwrap(), DeviceInitOutcome::ok());
        assert!(
            ads.time_series
                .as_ref()
                .unwrap()
                .inner
                .lock()
                .unwrap()
                .buf
                .is_some()
        );
    }

    /// The factory maps each TimeSeries DTYP to its base interface and arms the
    /// accumulation: a Passive waveform built through `universal_asyn_factory`
    /// gets a reprocess channel only via the time-series path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_series_factory_arms_accumulation_for_waveform() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("ts-factory-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "ts_factory".into(),
            interrupts,
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::asyn_record::register_port("ts_factory", handle, &crate::registry::open_owner())
            .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynInt32TimeSeries",
            inp: "@asyn(ts_factory,0)VAL",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");
        dev.set_record_info("TEST:TS:FACTORY", ScanType::Passive);
        let mut rec = WaveformRecord::new(4, DbFieldType::Long);
        dev.init(&mut rec).unwrap();
        assert!(
            dev.io_intr_receiver().is_some(),
            "factory must map asynInt32TimeSeries → asynInt32 and arm accumulation"
        );
    }

    #[test]
    fn test_io_intr_receiver_none_when_passive() {
        let mut ads = make_adapter(ScanType::Passive);
        assert!(ads.io_intr_receiver().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_io_intr_receiver_some_when_io_intr() {
        let mut ads = make_adapter(ScanType::IoIntr);
        // init() resolves drv_user_create → sets reason_set = true
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let rx = ads.io_intr_receiver();
        assert!(rx.is_some());
    }

    /// Build a `ScanType::IoIntr` adapter on the `asynUInt32Digital`
    /// interface with the given record `@asynMask`.
    fn make_uint32_io_intr_adapter(mask: u32) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-u32-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads =
            AsynDeviceSupport::from_handle(handle, link, "asynUInt32Digital").with_mask(mask);
        ads.set_record_info("TEST:U32", ScanType::IoIntr);
        ads
    }

    /// An open database gate the test binds to its record (`bind_interrupt_gate`
    /// is what `iocInit` does): the callbacks drop before `interruptAccept`
    /// (C parity), and a record with no gate reads its port's owner, which a
    /// hand-built port has none of.
    fn open_gate() -> Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate> {
        epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened()
    }

    /// Poll the interrupt FIFO until an entry arrives (bridge runs on a
    /// spawned task), returning its value. Panics if none arrives.
    async fn await_fifo_value(
        fifo: &Arc<std::sync::Mutex<InterruptFifo>>,
    ) -> crate::param::ParamValue {
        for _ in 0..200 {
            if let Some(entry) = fifo.lock().unwrap().pop().map(|d| d.entry) {
                return entry.value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("interrupt value never reached FIFO");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_uint32_masks_delivered_value_to_record_mask() {
        let gate = open_gate();
        // C uint32Callback delivers `pInterrupt->mask & value`
        // (asynPortDriver.cpp:729); the @asynMask=0x0F record must see
        // a 0xFF param value masked to 0x0F, matching the polled read.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        ads.bind_interrupt_gate(&gate);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0x01,
            ..Default::default()
        });

        match await_fifo_value(&fifo).await {
            crate::param::ParamValue::UInt32Digital(bits) => assert_eq!(
                bits, 0x0F,
                "I/O-Intr value must be masked to the record @asynMask"
            ),
            other => panic!("expected UInt32Digital, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_uint32_gates_out_non_overlapping_change() {
        let gate = open_gate();
        // C uint32Callback fires only when `pInterrupt->mask &
        // interruptMask` (asynPortDriver.cpp:720): a change confined to
        // bits outside the @asynMask=0x0F record (changed=0xF0) must be
        // gated out, while an overlapping change still arrives.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        ads.bind_interrupt_gate(&gate);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0xF0,
            ..Default::default()
        });
        // matches() rejects synchronously at notify(); a bounded wait
        // then proves the value was dropped, not merely delayed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            fifo.lock().unwrap().pop().is_none(),
            "non-overlapping change must be gated out, not delivered"
        );

        // The bridge is still live: an overlapping change arrives, proving
        // the gate (not a dead bridge) dropped the previous notify.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(0xFF),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0x01,
            ..Default::default()
        });
        assert!(matches!(
            await_fifo_value(&fifo).await,
            crate::param::ParamValue::UInt32Digital(0x0F)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_non_uint32_not_gated_by_default_mask() {
        let gate = open_gate();
        // Guard: the default 0xFFFFFFFF mask must NOT gate a non-UInt32
        // interrupt (uint32_changed_mask == 0). An asynInt32 I/O-Intr
        // record must receive its interrupt unchanged.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(42),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        assert!(
            matches!(
                await_fifo_value(&fifo).await,
                crate::param::ParamValue::Int32(42)
            ),
            "non-UInt32 interrupt must pass through ungated and unmasked"
        );
    }

    /// C `interruptCallbackInput` returns before the ring push while
    /// `interruptAccept` is false (devAsynInt32.c:556): a callback before
    /// the database runs leaves the ring empty and queues no wakeup, and the
    /// same callback after it rings once.
    /// C's average callbacks return before accumulating while `interruptAccept`
    /// is false (devAsynInt32.c:545-556, `:663`): a sample fired before the
    /// gate opens is dropped, not averaged, in both modes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_average_sample_before_the_gate_opens_is_dropped_in_both_modes() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        for mode1 in [false, true] {
            let gate = InterruptGate::new();
            let mut ads = make_average_adapter("asynInt32");
            ads.bind_interrupt_gate(&gate);
            if mode1 {
                ads.set_record_info("TEST:AVG", ScanType::IoIntr);
            }
            let mut rec = AiRecord::new(0.0);
            rec.sval = 4.0;
            ads.init(&mut rec).unwrap();
            let (reason, interrupts) = (ads.reason, ads.handle.interrupts().clone());
            let acc = ads.average.clone().unwrap();
            let fifo = ads.interrupt_fifo.clone();
            let _rx = ads.io_intr_receiver();
            let fire = |vals: &[i32]| {
                for v in vals {
                    interrupts.notify(InterruptValue {
                        reason,
                        addr: 0,
                        value: crate::param::ParamValue::Int32(*v),
                        timestamp: SystemTime::now(),
                        ..Default::default()
                    });
                }
            };

            fire(&[10, 20, 30, 40]);
            assert_eq!(acc.averager.count(), 0, "mode1={mode1}: closed: dropped");
            assert!(fifo.lock().unwrap().pop().is_none(), "mode1={mode1}");

            gate.set(true);
            fire(&[10, 20, 30, 40]);
            if mode1 {
                let v = await_fifo_value(&fifo).await;
                assert!(
                    matches!(v, crate::param::ParamValue::Int32(25)),
                    "open: the four samples average into the ring, got {v:?}"
                );
            } else {
                assert_eq!(acc.averager.count(), 4, "open: averaged");
            }
        }
    }

    /// C's enum callbacks return before `setEnums` and the DBE_PROPERTY post
    /// while `interruptAccept` is false (devAsynInt32.c:718/:732/:746/:760,
    /// devAsynUInt32Digital.c:553/:567/:581/:595). The gate is read when the
    /// callback fires: on a current-thread runtime the consumer task cannot
    /// drain before the gate opens, so a drain-time read would post it.
    #[tokio::test]
    async fn an_enum_table_before_the_gate_opens_is_not_applied_or_posted() {
        use crate::interrupt::InterruptValue;
        use crate::param::{EnumEntry, ParamValue};
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

        let gate = InterruptGate::new();
        let init: Arc<[EnumEntry]> =
            Arc::from(vec![enum_entry("OFF", 0, 0), enum_entry("ON", 1, 0)]);
        let mut ads = make_enum_adapter(init);
        ads.bind_interrupt_gate(&gate);
        let mut rec = MbbiRecord::new(0);
        ads.init(&mut rec).unwrap();
        let mut rx = ads.property_post_receiver().expect("armed");
        let table: Arc<[EnumEntry]> =
            Arc::from(vec![enum_entry("LOW", 0, 0), enum_entry("HIGH", 1, 1)]);
        let (reason, addr, interrupts) = (ads.reason, ads.addr, ads.handle.interrupts().clone());
        let fire = || {
            interrupts.notify(InterruptValue {
                reason,
                addr,
                value: ParamValue::Enum {
                    index: 0,
                    choices: table.clone(),
                },
                ..Default::default()
            });
        };

        fire();
        gate.set(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            rx.try_recv().is_err(),
            "fired closed, drained open: the table is not posted"
        );

        fire();
        let post = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("open: the table is applied and posted")
            .expect("channel open");
        assert_eq!(post.post_field, "VAL");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_callback_before_the_records_gate_opens_leaves_the_ring_empty() {
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        let gate = InterruptGate::new();
        let mut ads = make_uint32_io_intr_adapter(0xFF);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        ads.bind_interrupt_gate(&gate);
        let (reason, interrupts) = (ads.reason, ads.handle.interrupts().clone());
        let fifo = ads.interrupt_fifo.clone();
        let mut rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");
        let fire = |v: u32| {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::UInt32Digital(v),
                timestamp: SystemTime::now(),
                uint32_changed_mask: 0x01,
                ..Default::default()
            });
        };

        fire(1);
        assert!(fifo.lock().unwrap().waiting() == 0, "closed: no push");
        assert!(rx.try_recv().is_err(), "closed: no wakeup");

        gate.set(true);
        fire(2);
        assert_eq!(fifo.lock().unwrap().waiting(), 1, "open: one entry");
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "open: one wakeup");
    }

    /// Two records of two databases on one port: A's gate closed, B's open.
    /// A's callback leaves A's ring empty; B's delivers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_database_drops_its_records_callback_while_the_other_delivers() {
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        let (gate_a, gate_b) = (InterruptGate::new(), InterruptGate::opened());
        let mut a = make_uint32_io_intr_adapter(0xFF);
        let mut b = AsynDeviceSupport::from_handle(
            a.handle.clone(),
            AsynLink {
                port_name: "test".into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "VAL".into(),
            },
            "asynUInt32Digital",
        )
        .with_mask(0xFF);
        b.set_record_info("TEST:U32B", ScanType::IoIntr);
        let (mut rec_a, mut rec_b) = (LonginRecord::new(0), LonginRecord::new(0));
        a.init(&mut rec_a).unwrap();
        b.init(&mut rec_b).unwrap();
        a.bind_interrupt_gate(&gate_a);
        b.bind_interrupt_gate(&gate_b);
        let interrupts = a.handle.interrupts().clone();
        let (reason, fifo_a, fifo_b) =
            (a.reason, a.interrupt_fifo.clone(), b.interrupt_fifo.clone());
        let _rx_a = join_io_intr(&mut a, &mut rec_a).expect("A receiver");
        let _rx_b = join_io_intr(&mut b, &mut rec_b).expect("B receiver");

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::UInt32Digital(7),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0x01,
            ..Default::default()
        });
        assert!(fifo_a.lock().unwrap().waiting() == 0, "A is closed");
        assert_eq!(fifo_b.lock().unwrap().waiting(), 1, "B is open");
    }

    /// C `interruptCallbackInput` (devAsynInt32.c:564-576): every callback
    /// the record has not processed yet sits in the ring, in order, and only
    /// a full ring drops one - counted in `overflows`. The mailbox that used
    /// to sit between the driver and the ring kept the latest value per
    /// record-process and dropped the rest without a count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_rings_every_callback_before_the_record_reads() {
        let gate = open_gate();
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0xFF);
        ads.bind_interrupt_gate(&gate);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let mut rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");

        for v in [1u32, 2, 3] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::UInt32Digital(v),
                timestamp: SystemTime::now(),
                uint32_changed_mask: 0x01,
                ..Default::default()
            });
        }

        // The callback is inline in notify(): the ring is complete on return.
        let ringed: Vec<u32> = {
            let mut g = fifo.lock().unwrap();
            std::iter::from_fn(|| g.pop().map(|d| d.entry))
                .map(|e| match e.value {
                    crate::param::ParamValue::UInt32Digital(v) => v,
                    other => panic!("expected UInt32Digital, got {other:?}"),
                })
                .collect()
        };
        assert_eq!(
            ringed,
            vec![1, 2, 3],
            "every callback before the record reads is in the ring, oldest first"
        );
        assert_eq!(fifo.lock().unwrap().overflows, 0);
        // One scanIoRequest per fresh add (C: no request only on overflow).
        for _ in 0..3 {
            rx.try_recv()
                .expect("a process request per ringed callback");
        }
    }

    #[test]
    fn test_adapter_init_resolves_reason() {
        let mut ads = make_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.reason, 0); // "VAL" is param index 0
    }

    /// The output driver-callback ring must stay balanced: every armed
    /// readback cycle consumes exactly one ring entry, even when the cycle
    /// never reaches the read stage. C parity:
    /// `devAsynInt32.c::outputCallbackCallback` sets `newOutputCallbackValue`
    /// before `dbProcess` and, if the record did not process (PACT busy),
    /// calls `getCallbackValue` to discard the ring entry anyway. Without
    /// this fallback a start callback that bails on the PACT entry guard
    /// (it raced the bo's own put) leaves its value stranded, the finalize
    /// callback pops the stale start value, and the finalize value is never
    /// popped — the AD `Acquire` bo getting stuck at 1 after a fast acquire.
    /// Boundary cases: bail-with-entries, read-clears-flag, empty-ring,
    /// and the input gate (non-asyn:READBACK is never armed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readback_reconcile_keeps_ring_balanced_on_bail() {
        use epics_base_rs::server::records::bo::BoRecord;

        fn push(ads: &AsynDeviceSupport, v: i32) {
            ads.interrupt_fifo
                .lock()
                .unwrap()
                .push_with_overflow(CachedInterrupt {
                    value: crate::param::ParamValue::Int32(v),
                    timestamp: SystemTime::UNIX_EPOCH,
                    alarm_status: 0,
                    alarm_severity: 0,
                    aux_status: crate::error::AsynStatus::Success,
                    octet_nul: false,
                });
        }
        let len = |ads: &AsynDeviceSupport| ads.interrupt_fifo.lock().unwrap().waiting();

        // asyn:READBACK output adapter; init so reason_set is true (read pops).
        let mut ads = make_adapter(ScanType::Passive);
        let mut rec = BoRecord::new(1);
        ads.init(&mut rec).unwrap();
        ads.set_asyn_readback(true);

        // Two output callbacks arrive: start=1 then finalize=0.
        push(&ads, 1);
        push(&ads, 0);
        assert_eq!(len(&ads), 2);

        // Cycle 1 — armed but the process bails before read() (the PACT entry
        // guard fired). reconcile must discard the oldest (start) entry so the
        // ring stays balanced: one callback == one consumed entry.
        ads.arm_readback_callback();
        ads.reconcile_readback_callback();
        assert_eq!(len(&ads), 1, "a bailed cycle must still consume one entry");

        // Cycle 2 — armed and the process reaches read(): it pops the finalize
        // 0 into VAL and clears the armed flag, so reconcile is a no-op.
        ads.arm_readback_callback();
        ads.read(&mut rec).unwrap();
        assert_eq!(len(&ads), 0, "read() pops the finalize entry");
        ads.reconcile_readback_callback();
        assert_eq!(
            len(&ads),
            0,
            "reconcile after a real read must not double-pop"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Enum(0)),
            "VAL reads the finalize 0 back (Done)"
        );

        // Empty-ring boundary: an armed bail with nothing queued is a no-op.
        ads.arm_readback_callback();
        ads.reconcile_readback_callback();
        assert_eq!(len(&ads), 0);

        // Input gate: a non-asyn:READBACK device (plain I/O Intr / input) is
        // never armed, so reconcile must NOT discard — inputs use the
        // scan/RPRO reprocess model, not the output discard fallback.
        let mut input = make_adapter(ScanType::IoIntr);
        push(&input, 9);
        input.arm_readback_callback();
        input.reconcile_readback_callback();
        assert_eq!(len(&input), 1, "input I/O Intr must not discard on a bail");
    }

    // --- ai LINEAR ESLO/EOFF from getBounds (C devAsynInt32.c::convertAi) ---

    /// C `convertAi` formula: ESLO=(EGUF-EGUL)/(high-low),
    /// EOFF=(high*EGUL-low*EGUF)/(high-low). With low=0, high=4095,
    /// EGUL=0.0, EGUF=10.0 → ESLO ≈ 10/4095 ≈ 0.002442, EOFF=0.0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_eslo_eoff_filled_from_get_bounds_int32() {
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        // Configure EGUL/EGUF before init so convertAi has them.
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2; // LINEAR (LINR codes: 0=NO_CONVERSION, 1=SLOPE, 2=LINEAR)
        ads.init(&mut rec).unwrap();

        let eslo = match rec.get_field("ESLO").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let eoff = match rec.get_field("EOFF").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (eslo - 10.0 / 4095.0).abs() < 1e-9,
            "ESLO must equal (EGUF-EGUL)/(high-low): got {eslo}"
        );
        assert!(
            eoff.abs() < 1e-9,
            "EOFF must equal 0 for symmetric range: got {eoff}"
        );
    }

    /// EGUF=10, EGUL=-10 with bounds [-2048, 2047] → ESLO ≈ 20/4095.
    /// EOFF = (2047*-10 - -2048*10)/4095 = (-20470 + 20480)/4095 = 10/4095.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_eslo_eoff_signed_range() {
        let mut ads = make_bounded_adapter(-2048, 2047, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 10.0;
        rec.egul = -10.0;
        rec.linr = 2;
        ads.init(&mut rec).unwrap();
        let eslo = match rec.get_field("ESLO").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let eoff = match rec.get_field("EOFF").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        let denom = (2047 - -2048) as f64;
        assert!((eslo - 20.0 / denom).abs() < 1e-9);
        let expected_eoff = (2047.0 * -10.0 - -2048.0 * 10.0) / denom;
        assert!(
            (eoff - expected_eoff).abs() < 1e-9,
            "EOFF expected {expected_eoff} got {eoff}"
        );
    }

    /// Driver returning low==high (no usable range) must leave the
    /// record's ESLO/EOFF unchanged — matches C `convertAi:444`
    /// (`if (deviceHigh != deviceLow)`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_skip_when_bounds_equal() {
        let mut ads = make_bounded_adapter(0, 0, "asynInt32");
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2;
        // Pre-set ESLO/EOFF to sentinel values to detect untouched.
        rec.eslo = 123.456;
        rec.eoff = 42.0;
        ads.init(&mut rec).unwrap();
        assert!((rec.eslo - 123.456).abs() < 1e-9);
        assert!((rec.eoff - 42.0).abs() < 1e-9);
    }

    /// Records without an ESLO field (e.g. longin) must skip the
    /// LINEAR wiring entirely — the `record.get_field("ESLO").is_some()`
    /// guard in `init()` prevents a wasted GetBoundsInt32 round-trip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn longin_skips_linear_wiring() {
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        // Should not error or block — just no-ops because LonginRecord
        // doesn't expose ESLO.
        ads.init(&mut rec).unwrap();
    }

    /// A port that does not override getBounds is asynPortDriver's 0..65535
    /// (asynPortDriver.cpp:2073-2074), and initAi runs convertAi whatever LINR
    /// is (devAsynInt32.c:828): an ai left at EGUF = EGUL = 0, as pilatus's
    /// PixelCutOff_RBV, gets ESLO = 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_eslo_from_the_asyn_port_driver_default_bounds() {
        let mut ads = make_adapter(ScanType::Passive); // TestPort: default bounds
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        assert_eq!((rec.eslo, rec.eoff), (0.0, 0.0));

        let mut ads = make_adapter(ScanType::Passive);
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 65535.0;
        rec.linr = 2; // LINEAR
        ads.init(&mut rec).unwrap();
        assert_eq!((rec.eslo, rec.eoff), (1.0, 0.0));
    }

    /// C `devAsynInt32::processAi` (devAsynInt32.c:848-851) sets
    /// `pr->rval = value` and returns `0`, so the ai record's own
    /// `convert()` runs — applying ROFF/ASLO/AOFF and the LINR
    /// linearisation (ESLO/EOFF). The asyn-rs adapter must therefore route
    /// the raw count through RVAL and return `ok()` (run convert), NOT
    /// `computed()` (skip convert). Before the fix the read wrote VAL
    /// directly + returned `computed()`, so the init-computed ESLO/EOFF
    /// were dead and a LINEAR ai surfaced raw counts.
    #[test]
    fn int32_ai_linear_read_applies_eslo_eoff() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        let mut rec = AiRecord::new(0.0);
        // EGUF/EGUL/LINR before init so init's convertAi fills ESLO/EOFF:
        // ESLO=(10-0)/(4095-0)=10/4095, EOFF=0.
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2; // LINEAR
        ads.init(&mut rec).unwrap();
        // Drive the I/O Intr read path with a raw full-scale count.
        ads.set_record_info("TEST:AI", ScanType::IoIntr);
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(4095, 0));

        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            !outcome.did_compute(),
            "asynInt32 ai routes raw→RVAL and runs the record convert (C return 0)"
        );
        // Framework runs the record convert when did_compute is false.
        rec.process().unwrap();
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 10.0).abs() < 1e-6,
            "LINEAR ESLO/EOFF applied at read: 4095*(10/4095) = 10.0, got {val}"
        );
    }

    /// LINR=NO_CONVERSION ai: the record convert still runs (C returns 0
    /// unconditionally) but applies no ESLO/EOFF, so VAL == raw count —
    /// confirming the raw→RVAL + run-convert path preserves the identity
    /// case the old direct-VAL path produced.
    #[test]
    fn int32_ai_no_conversion_read_passes_raw() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        let mut rec = AiRecord::new(0.0);
        rec.linr = 0; // NO_CONVERSION
        ads.init(&mut rec).unwrap();
        ads.set_record_info("TEST:AI", ScanType::IoIntr);
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(1234, 0));

        let outcome = ads.read(&mut rec).unwrap();
        assert!(!outcome.did_compute());
        rec.process().unwrap();
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 1234.0).abs() < 1e-9,
            "NO_CONVERSION ai: VAL == raw count, got {val}"
        );
    }

    /// A record without an ESLO field (longin) is not an ai — the
    /// `get_field("ESLO").is_some()` discriminator keeps the direct VAL
    /// path and `computed()` (skip convert), unchanged from before. This
    /// scopes the convert-routing fix to ai records; mbbi/bi/longin (no
    /// ESLO) are unaffected.
    #[test]
    fn int32_longin_read_stays_direct_computed() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_bounded_adapter(0, 4095, "asynInt32");
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        ads.set_record_info("TEST:LI", ScanType::IoIntr);
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(77, 0));

        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "non-ai (no ESLO) keeps the direct VAL path + computed()"
        );
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Long(v) => v,
            _ => panic!(),
        };
        assert_eq!(val, 77, "longin VAL set directly to the raw count");
    }

    // --- averaging device support (asynInt32Average / asynFloat64Average)
    //     C interruptCallbackAverage + processAiAverage, periodic-SCAN model ---

    /// asynInt32Average: the always-on interrupt accumulates samples; the
    /// periodic process drains the rounded arithmetic mean to RVAL and runs
    /// the ai convert (C `processAiAverage` `pr->rval = dval; return 0`,
    /// devAsynInt32.c:906-910). Mean of [10,20,30,40] = 25 → RVAL → VAL
    /// (LINR=NO_CONVERSION). The drain resets the accumulator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_int32_drains_rounded_mean_to_rval_and_resets() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.bind_interrupt_gate(&gate);
        let mut rec = AiRecord::new(0.0);
        rec.linr = 0; // NO_CONVERSION: VAL == RVAL after convert
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();

        for v in [10, 20, 30, 40] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_average_count(&acc, 4).await;

        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            !outcome.did_compute(),
            "int32 average routes the mean→RVAL and runs the ai convert (C return 0)"
        );
        let rval = match rec.get_field("RVAL").unwrap() {
            EpicsValue::Long(v) => v,
            _ => panic!(),
        };
        assert_eq!(rval, 25, "mean of [10,20,30,40] = 25 → RVAL");
        rec.process().unwrap();
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 25.0).abs() < 1e-9,
            "NO_CONVERSION ai: VAL == RVAL = 25"
        );
        assert_eq!(
            acc.averager.count(),
            0,
            "the drain must reset the accumulator (C numAverage=0, sum=0)"
        );
    }

    /// asynFloat64Average: the periodic process drains the mean directly to
    /// VAL (C `processAiAverage` applies ASLO/AOFF/SMOO and returns `2`,
    /// devAsynFloat64.c:727-735). Mean of [1,2,6] = 3.0. ASLO=1/AOFF=0/SMOO=0
    /// leave the mean unscaled. The drain resets the accumulator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_float64_drains_mean_to_val_and_resets() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        ads.bind_interrupt_gate(&gate);
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        for v in [1.0, 2.0, 6.0] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Float64(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_average_count(&acc, 3).await;

        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "float64 average sets VAL directly and skips the convert (C return 2)"
        );
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!((val - 3.0).abs() < 1e-9, "mean of [1,2,6] = 3.0 → VAL");
        assert_eq!(
            acc.averager.count(),
            0,
            "the drain must reset the accumulator"
        );
    }

    /// Zero samples since the last process: C `processAiAverage` sets
    /// `UDF_ALARM`/`INVALID` and returns `-2` — VAL is left untouched
    /// (devAsynFloat64.c:721-725). No averaging into a zero-divide, no
    /// stale-value reuse.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_zero_samples_raises_udf_invalid_and_keeps_val() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        let mut rec = AiRecord::new(7.5); // pre-existing VAL
        ads.init(&mut rec).unwrap();
        // No samples notified → the accumulator is empty.
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "zero-samples skips the convert so VAL is not overwritten from RVAL"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((
                epics_base_rs::server::recgbl::alarm_status::UDF_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid as u16
            )),
            "zero-samples must raise UDF_ALARM/INVALID (C return -2)"
        );
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 7.5).abs() < 1e-9,
            "zero-samples must leave VAL untouched, got {val}"
        );
    }

    /// The driver alarm carried by a sample propagates through the average:
    /// `interruptCallbackAverage` captures `alarmStatus`/`alarmSeverity`
    /// (devAsynInt32.c:705-707) and `processAiAverage` applies it
    /// (recGblSetSevr, :915-918). A COMM/INVALID sample must surface on read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_propagates_last_sample_driver_alarm() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        ads.bind_interrupt_gate(&gate);
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Float64(5.0),
            timestamp: SystemTime::now(),
            alarm_status: 9,   // COMM_ALARM
            alarm_severity: 3, // INVALID
            ..Default::default()
        });
        await_average_count(&acc, 1).await;

        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((9, 3)),
            "the sample's driver alarm (COMM/INVALID) must propagate on read"
        );
    }

    /// A transport-error period (a sample carries `aux_status != Success`):
    /// C `processAiAverage` discards the averaged value (RVAL/VAL stay stale,
    /// return -1) and raises the transport-mapped alarm via
    /// `asynStatusToEpicsAlarm` (devAsynInt32.c:919-927, devAsynFloat64.c:736-754;
    /// asynEpicsUtils.c:238-265). The period is still consumed (sum/numAverage
    /// reset before the status check), so the next read sees no samples.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_transport_error_discards_value_and_raises_mapped_alarm() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        ads.bind_interrupt_gate(&gate);
        let mut rec = AiRecord::new(42.0); // pre-existing VAL
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        // A sample with a transport error (Timeout) during the period.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Float64(5.0),
            timestamp: SystemTime::now(),
            aux_status: crate::error::AsynStatus::Timeout,
            ..Default::default()
        });
        await_average_count(&acc, 1).await;

        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "transport-error period skips the store (C return -1)"
        );
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 42.0).abs() < 1e-9,
            "transport error must discard the averaged value (VAL stays stale), got {val}"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((10, 3)),
            "transport Timeout must map to TIMEOUT_ALARM/INVALID"
        );
        assert_eq!(
            acc.averager.count(),
            0,
            "the period is consumed even on a transport error (sum/numAverage reset)"
        );

        // The accumulated transport status reset with the period: a subsequent
        // read with no samples is the ordinary zero-samples UDF case.
        let outcome2 = ads.read(&mut rec).unwrap();
        assert!(outcome2.did_compute());
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((
                epics_base_rs::server::recgbl::alarm_status::UDF_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid as u16
            )),
            "transport status reset → next empty read is zero-samples UDF, not a stale transport error"
        );
    }

    /// C `asynStatusToEpicsAlarm` only fills a still-`NO_ALARM` field, so a
    /// sample's own EPICS alarm wins over the transport-status mapping
    /// (asynEpicsUtils.c:242-264). A Timeout transport status with a sample
    /// EPICS alarm of COMM/MAJOR surfaces as COMM/MAJOR — not TIMEOUT/INVALID —
    /// while the transport error still gates the store off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_sample_epics_alarm_wins_over_transport_mapping() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        ads.bind_interrupt_gate(&gate);
        let mut rec = AiRecord::new(42.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Float64(5.0),
            timestamp: SystemTime::now(),
            aux_status: crate::error::AsynStatus::Timeout, // would map to (10, 3)
            alarm_status: 9,                               // COMM_ALARM (sample's own)
            alarm_severity: 2,                             // MAJOR
            ..Default::default()
        });
        await_average_count(&acc, 1).await;

        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((9, 2)),
            "the sample's EPICS alarm (COMM/MAJOR) must win over the transport Timeout mapping"
        );
        let val = match rec.get_field("VAL").unwrap() {
            EpicsValue::Double(v) => v,
            _ => panic!(),
        };
        assert!(
            (val - 42.0).abs() < 1e-9,
            "the transport error still discards the value regardless of which alarm wins"
        );
    }

    // --- asyn Average Mode 1 (SCAN="I/O Intr" SVAL-decimation,
    //     C `interruptCallbackAverage` isIOIntrScan branch) ---

    /// Mode 1 decimates the running mean into the IoIntr ring every
    /// `round(SVAL)` samples (C devAsynInt32.c:673-702): below the threshold
    /// the ring stays empty; the `round(SVAL)`-th sample pushes the mean and
    /// resets the accumulator, and each decimation wakes `io_intr_receiver`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_mode1_io_intr_decimates_every_sval_samples_into_ring() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.bind_interrupt_gate(&gate);
        ads.set_record_info("TEST:AVG", ScanType::IoIntr); // Mode 1
        let mut rec = AiRecord::new(0.0);
        rec.sval = 4.0; // numToAverage = round(4.0) = 4
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        let fifo = ads.interrupt_fifo.clone();

        assert!(ads.io_intr_receiver().is_some());

        // 3 samples (< SVAL=4): accumulate, no decimation, ring stays empty.
        for v in [10, 20, 30] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                ..Default::default()
            });
        }
        await_average_count(&acc, 3).await;
        assert!(
            fifo.lock().unwrap().pop().is_none(),
            "3 < SVAL=4: no decimation yet, the ring is empty"
        );

        // 4th sample reaches the threshold: mean of [10,20,30,40] = 25 is
        // pushed and the accumulator resets.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(40),
            timestamp: SystemTime::now(),
            ..Default::default()
        });
        let value = await_fifo_value(&fifo).await;
        assert!(
            matches!(value, crate::param::ParamValue::Int32(25)),
            "the SVAL-th sample decimates the rounded mean (round((10+20+30+40)/4) = 25) into the ring, got {value:?}"
        );
        assert_eq!(
            acc.averager.count(),
            0,
            "decimation resets the accumulator (C numAverage=0, sum=0)"
        );
    }

    /// The one average callback asks `isIOIntrScan` per sample
    /// (devAsynInt32.c:673-707), and `get_ioint_info` sets and clears it
    /// (:383/:399): a Passive average moved to I/O Intr decimates into the
    /// ring and wakes the receiver iocInit took, and moved back it
    /// accumulates for the periodic read again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_average_follows_its_scan_moves() {
        use epics_base_rs::server::device_support::IoIntrCmd;
        use epics_base_rs::server::record::CommonFields;
        use epics_base_rs::server::records::ai::AiRecord;
        let gate = open_gate();
        let mut ads = make_average_adapter("asynInt32");
        ads.bind_interrupt_gate(&gate);
        ads.set_record_info("TEST:AVG", ScanType::Passive);
        let mut rec = AiRecord::new(0.0);
        rec.sval = 2.0;
        ads.init(&mut rec).unwrap();
        let mut rx = ads.io_intr_receiver().expect("an average's ioScanPvt");
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        let fifo = ads.interrupt_fifo.clone();
        let sample = |v| InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(v),
            timestamp: SystemTime::now(),
            ..Default::default()
        };
        let mut common = CommonFields::default();

        assert!(ads.get_ioint_info(&mut rec, &mut common, IoIntrCmd::Add));
        interrupts.notify(sample(10));
        interrupts.notify(sample(20));
        let value = await_fifo_value(&fifo).await;
        assert!(
            matches!(value, crate::param::ParamValue::Int32(15)),
            "on I/O Intr, SVAL=2 samples decimate, got {value:?}"
        );
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the decimation wakes the iocInit receiver");

        assert!(ads.get_ioint_info(&mut rec, &mut common, IoIntrCmd::Delete));
        interrupts.notify(sample(30));
        interrupts.notify(sample(40));
        await_average_count(&acc, 2).await;
        assert!(
            fifo.lock().unwrap().pop().is_none(),
            "off I/O Intr the samples accumulate for the periodic read"
        );
    }

    /// C floors the decimation count at 1: `numToAverage = (int)(sval+0.5); if
    /// (numToAverage < 1) numToAverage = 1` (devAsynInt32.c:674-675). With
    /// SVAL=0 every single sample decimates (the mean of one sample is itself).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_mode1_sval_below_one_floors_to_one_sample() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.bind_interrupt_gate(&gate);
        ads.set_record_info("TEST:AVG", ScanType::IoIntr);
        let mut rec = AiRecord::new(0.0);
        rec.sval = 0.0; // round(0.0) = 0 → floored to 1
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        assert!(ads.io_intr_receiver().is_some());

        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(42),
            timestamp: SystemTime::now(),
            ..Default::default()
        });
        let value = await_fifo_value(&fifo).await;
        assert!(
            matches!(value, crate::param::ParamValue::Int32(42)),
            "SVAL=0 floors to 1: every sample decimates, mean of one sample is itself, got {value:?}"
        );
    }

    /// The decimated ring entry carries the TRIGGERING sample's transport
    /// status and EPICS alarm — C `rp->status = pasynUser->auxStatus`
    /// (devAsynInt32.c:685-687), NOT the Mode 2 OR-accumulation. Earlier
    /// samples' error statuses are summed into the *value* but do not taint the
    /// entry's status: only the triggering (4th) sample's status rides the ring.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_mode1_decimated_entry_carries_triggering_sample_status() {
        let gate = open_gate();
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.bind_interrupt_gate(&gate);
        ads.set_record_info("TEST:AVG", ScanType::IoIntr);
        let mut rec = AiRecord::new(0.0);
        rec.sval = 4.0;
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        assert!(ads.io_intr_receiver().is_some());

        // Samples 1-3 carry a transport Timeout; they are still summed into the
        // mean, but their status must NOT survive to the ring entry.
        for v in [10, 20, 30] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(v),
                timestamp: SystemTime::now(),
                aux_status: crate::error::AsynStatus::Timeout,
                ..Default::default()
            });
        }
        // The 4th (triggering) sample is clean (Success) but carries its own
        // EPICS alarm; the ring entry must reflect THIS sample.
        interrupts.notify(InterruptValue {
            reason,
            addr: 0,
            value: crate::param::ParamValue::Int32(40),
            timestamp: SystemTime::now(),
            aux_status: crate::error::AsynStatus::Success,
            alarm_status: 9,   // COMM_ALARM (triggering sample's own)
            alarm_severity: 2, // MAJOR
            ..Default::default()
        });

        // Pop the full decimated entry (await_fifo_value returns only the value).
        let entry = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(e) = fifo.lock().unwrap().pop().map(|d| d.entry) {
                    got = Some(e);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            got.expect("decimated entry never reached the ring")
        };
        assert!(
            matches!(entry.value, crate::param::ParamValue::Int32(25)),
            "all four samples are summed into the mean (round((10+20+30+40)/4) = 25), got {:?}",
            entry.value
        );
        assert_eq!(
            entry.aux_status,
            crate::error::AsynStatus::Success,
            "the entry takes the triggering sample's transport status (Success), not the OR of the earlier Timeouts"
        );
        assert_eq!(
            (entry.alarm_status, entry.alarm_severity),
            (9, 2),
            "the entry takes the triggering sample's EPICS alarm (COMM/MAJOR)"
        );
    }

    // --- driver enum-string table -> record state fields
    //     (C devAsynInt32.c::initCommon enum block + setEnums) ---

    /// Build an `asynEnum` adapter whose driver exposes a `MODE` enum param
    /// carrying `choices`. `init()` reads this table and pushes it onto the
    /// bound record's state fields.
    fn make_enum_adapter(choices: std::sync::Arc<[crate::param::EnumEntry]>) -> AsynDeviceSupport {
        make_enum_adapter_as(choices, "asynEnum")
    }

    /// The same port, bound through `dtyp`.
    fn make_enum_adapter_as(
        choices: std::sync::Arc<[crate::param::EnumEntry]>,
        dtyp: &str,
    ) -> AsynDeviceSupport {
        struct EnumPort {
            base: PortDriverBase,
        }
        impl PortDriver for EnumPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new(
            "test_enum",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("MODE", ParamType::Enum).unwrap();
        base.set_enum_choices_param(0, 0, choices).unwrap();
        let driver = EnumPort { base };

        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-enum-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test_enum".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test_enum".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "MODE".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, dtyp);
        ads.set_record_info("TEST:ENUM", ScanType::Passive);
        ads
    }

    fn enum_entry(s: &str, value: i32, severity: u16) -> crate::param::EnumEntry {
        crate::param::EnumEntry {
            string: s.into(),
            value,
            severity,
        }
    }

    fn field_string(rec: &dyn Record, name: &str) -> String {
        match rec.get_field(name).unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            other => panic!("{name} not a String: {other:?}"),
        }
    }

    /// C `devAsynInt32::initCommon` (298-324) reads the driver's asynEnum
    /// table and `setEnums` (415-435) copies strings/values/severities onto
    /// the mbbi state fields ZRST/ZRVL/ZRSV…. Before the fix the actor
    /// dropped the table (`let (idx, _entries)`) so the record kept its .db
    /// strings.
    #[test]
    fn mbbi_init_propagates_driver_enum_table() {
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let choices: std::sync::Arc<[crate::param::EnumEntry]> =
            std::sync::Arc::from(vec![enum_entry("OFF", 0, 0), enum_entry("ON", 5, 2)]);
        let mut ads = make_enum_adapter(choices);
        let mut rec = MbbiRecord::new(0);
        // A surplus .db string on a state the driver does not define must be
        // blanked, matching C setEnums zeroing all numOut slots first.
        rec.put_field("TWST", EpicsValue::String("STALE".into()))
            .unwrap();

        ads.init(&mut rec).unwrap();

        assert_eq!(field_string(&rec, "ZRST"), "OFF");
        assert_eq!(field_string(&rec, "ONST"), "ON");
        assert_eq!(
            rec.get_field("ZRVL").unwrap(),
            EpicsValue::ULong(0),
            "ZRVL from entry 0 value"
        );
        assert_eq!(
            rec.get_field("ONVL").unwrap(),
            EpicsValue::ULong(5),
            "ONVL from entry 1 value"
        );
        assert_eq!(
            rec.get_field("ZRSV").unwrap(),
            EpicsValue::Short(0),
            "ZRSV from entry 0 severity"
        );
        assert_eq!(
            rec.get_field("ONSV").unwrap(),
            EpicsValue::Short(2),
            "ONSV from entry 1 severity"
        );
        assert_eq!(
            field_string(&rec, "TWST"),
            "",
            "state beyond the driver table is cleared (C setEnums zeroes numOut)"
        );
    }

    /// C `initBi` (devAsynInt32.c:1138-1140) passes `maxEnums=2`,
    /// `&pr->znam`, NULL values, `&pr->zsv`: bi/bo get only ZNAM/ONAM and
    /// ZSV/OSV — no raw-value fields.
    #[test]
    fn bi_init_propagates_znam_onam_and_severities() {
        use epics_base_rs::server::records::bi::BiRecord;
        let choices: std::sync::Arc<[crate::param::EnumEntry]> =
            std::sync::Arc::from(vec![enum_entry("CLOSED", 0, 0), enum_entry("OPEN", 1, 1)]);
        let mut ads = make_enum_adapter(choices);
        let mut rec = BiRecord::new(0);

        ads.init(&mut rec).unwrap();

        assert_eq!(field_string(&rec, "ZNAM"), "CLOSED");
        assert_eq!(field_string(&rec, "ONAM"), "OPEN");
        assert_eq!(
            rec.get_field("ZSV").unwrap(),
            EpicsValue::Short(0),
            "ZSV from entry 0 severity"
        );
        assert_eq!(
            rec.get_field("OSV").unwrap(),
            EpicsValue::Short(1),
            "OSV from entry 1 severity"
        );
    }

    /// A record exposing no enum state fields (ai) must be left untouched —
    /// `apply_enum_table` returns early and `init` performs no enum read.
    #[test]
    fn ai_init_ignores_enum_table() {
        use epics_base_rs::server::records::ai::AiRecord;
        let choices: std::sync::Arc<[crate::param::EnumEntry]> =
            std::sync::Arc::from(vec![enum_entry("X", 0, 0)]);
        let mut ads = make_enum_adapter(choices);
        let mut rec = AiRecord::new(0.0);
        // Must not panic or error; ai has no ZRST/ZNAM so nothing is written.
        ads.init(&mut rec).unwrap();
        assert!(rec.get_field("ZRST").is_none());
        assert!(rec.get_field("ZNAM").is_none());
    }

    /// The `enum_table_fields` producer (shared by init + runtime callback)
    /// yields the full mbb state block: string/value/severity per slot, with
    /// surplus slots cleared (empty / 0 / 0) — C `setEnums` semantics.
    #[test]
    fn enum_table_fields_mbb_shape() {
        let entries = vec![enum_entry("OFF", 0, 0), enum_entry("ON", 5, 2)];
        let fields = enum_table_fields(EnumRecordShape::Mbb, &entries);
        // 16 states x (string + value + severity) = 48 entries.
        assert_eq!(fields.len(), 48);
        let get = |name: &str| {
            fields
                .iter()
                .find(|(f, _)| f == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("ZRST"), Some(EpicsValue::String("OFF".into())));
        assert_eq!(get("ONST"), Some(EpicsValue::String("ON".into())));
        assert_eq!(get("ZRVL"), Some(EpicsValue::ULong(0)));
        assert_eq!(get("ONVL"), Some(EpicsValue::ULong(5)));
        assert_eq!(get("ZRSV"), Some(EpicsValue::Short(0)));
        assert_eq!(get("ONSV"), Some(EpicsValue::Short(2)));
        // Surplus state beyond the driver table is blanked.
        assert_eq!(get("TWST"), Some(EpicsValue::String("".into())));
        assert_eq!(get("TWVL"), Some(EpicsValue::ULong(0)));
    }

    /// bi/bo shape: 2 states, ZNAM/ONAM + ZSV/OSV, no raw-value fields.
    #[test]
    fn enum_table_fields_bi_shape() {
        let entries = vec![enum_entry("CLOSED", 0, 0), enum_entry("OPEN", 1, 1)];
        let fields = enum_table_fields(EnumRecordShape::Bi, &entries);
        // 2 states x (string + severity) = 4 entries; no value fields.
        assert_eq!(fields.len(), 4);
        assert!(!fields.iter().any(|(f, _)| f == "ZRVL" || f == "ONVL"));
        let get = |name: &str| {
            fields
                .iter()
                .find(|(f, _)| f == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("ZNAM"), Some(EpicsValue::String("CLOSED".into())));
        assert_eq!(get("ONAM"), Some(EpicsValue::String("OPEN".into())));
        assert_eq!(get("ZSV"), Some(EpicsValue::Short(0)));
        assert_eq!(get("OSV"), Some(EpicsValue::Short(1)));
    }

    /// C `setEnums` keeps 16 states of 25-byte strings (devAsynInt32.c:65-66),
    /// so a driver table past either limit equals what the ring carries back:
    /// a value-only interrupt posts nothing, and a real change still does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_driver_table_past_the_setenums_limits_posts_nothing_on_a_value_change() {
        use crate::interrupt::InterruptValue;
        use crate::param::{EnumEntry, ParamValue};
        use epics_base_rs::server::records::mbbi::MbbiRecord;

        let long = "A_VERY_LONG_STATE_NAME_OF_THIRTY";
        let mut table = vec![enum_entry(long, 0, 0), enum_entry("ON", 1, 0)];
        for i in 2..30 {
            table.push(enum_entry(&format!("S{i}"), i, 0));
        }
        let table: Arc<[EnumEntry]> = Arc::from(table);
        let mut ads = make_enum_adapter(table.clone());
        let gate = open_gate();
        ads.bind_interrupt_gate(&gate);
        let mut rec = MbbiRecord::new(0);
        ads.init(&mut rec).unwrap();
        let mut rx = ads.property_post_receiver().expect("armed");

        let notify = |ads: &AsynDeviceSupport, index, choices: Arc<[EnumEntry]>| {
            ads.handle.interrupts().notify(InterruptValue {
                reason: ads.reason,
                addr: ads.addr,
                value: ParamValue::Enum { index, choices },
                ..Default::default()
            });
        };
        notify(&ads, 1, table.clone());
        // The mailbox keeps the latest value, so let the receiver drain this
        // one before the change replaces it.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "a value-only interrupt posts no DBE_PROPERTY"
        );
        let changed: Arc<[EnumEntry]> = Arc::from(vec![enum_entry("LOW", 0, 0)]);
        notify(&ads, 0, changed);
        // The value-only interrupt was consumed before the change arrived; a
        // coalesce would mean the change replaced it unread and the silence
        // above proved nothing.
        assert_eq!(
            ads.handle.interrupts().coalesce_count(),
            0,
            "the value-only interrupt must be drained before the change"
        );
        let post = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the table change posts")
            .expect("channel open");
        assert!(
            post.writes
                .iter()
                .any(|(f, v)| f == "ZRST" && *v == EpicsValue::String("LOW".into())),
            "the first post is the real change, not the value-only interrupt: {:?}",
            post.writes
        );
    }

    /// Runtime asynEnum re-propagation: after init captures the table, a
    /// driver enum-table change delivered through the interrupt path must
    /// surface on the `property_post_receiver` channel as the new mbb field
    /// block — but an interrupt carrying the SAME choices (a value-only
    /// change) must fire NO post (C's asynEnum callback only runs on a table
    /// change, never on an int32 value callback).
    /// C `initCommon` registers `callbackEnum` in `init_record`, right after
    /// `setEnums` (devAsynInt32.c:317-323); iocInit's `property_post_receiver`
    /// only takes that registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enum_registers_in_init() {
        use crate::param::EnumEntry;
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let choices: Arc<[EnumEntry]> = Arc::from(vec![enum_entry("OFF", 0, 0)]);
        let mut ads = make_enum_adapter(choices);
        ads.init(&mut MbbiRecord::new(0)).unwrap();
        assert!(ads.enum_interrupt_sub.is_some(), "after init");
        assert!(ads.property_post_receiver().is_some(), "at iocInit");
        assert!(ads.property_post_receiver().is_none(), "registered once");
    }

    /// C enters the asynEnum block on `findInterface(asynEnumType) &&
    /// maxEnums>0` and registers `callbackEnum` even when
    /// `pasynEnumSyncIO->read` fails (devAsynInt32.c:298-322); a port without
    /// asynEnum registers nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enum_registers_without_a_table() {
        use crate::interfaces::Capability;
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        for (enum_iface, registered) in [(true, true), (false, false)] {
            let mut ads = make_adapter(ScanType::Passive);
            if !enum_iface {
                let caps: Vec<Capability> = InterfaceMask::UNMIGRATED_ALL
                    .capabilities()
                    .into_iter()
                    .filter(|c| c.interface_type() != InterfaceType::Enum)
                    .collect();
                ads.handle.set_interfaces(caps);
            }
            ads.init(&mut MbbiRecord::new(0)).unwrap();
            assert!(ads.enum_choices.is_none(), "the Int32 param has no table");
            assert_eq!(ads.enum_interrupt_sub.is_some(), registered, "{enum_iface}");
            assert_eq!(ads.property_post_receiver().is_some(), registered);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn property_post_receiver_emits_only_on_table_change() {
        use crate::interrupt::InterruptValue;
        use crate::param::{EnumEntry, ParamValue};
        use epics_base_rs::server::records::mbbi::MbbiRecord;

        let init_choices: Arc<[EnumEntry]> =
            Arc::from(vec![enum_entry("OFF", 0, 0), enum_entry("ON", 1, 0)]);
        let mut ads = make_enum_adapter(init_choices.clone());
        let gate = open_gate();
        ads.bind_interrupt_gate(&gate);
        let mut rec = MbbiRecord::new(0);
        ads.init(&mut rec).unwrap();

        let mut rx = ads
            .property_post_receiver()
            .expect("enum record arms the property callback");

        // 1. A value-only interrupt (same choices) → no post.
        ads.handle.interrupts().notify(InterruptValue {
            reason: ads.reason,
            addr: ads.addr,
            value: ParamValue::Enum {
                index: 1,
                choices: init_choices.clone(),
            },
            ..Default::default()
        });

        // 2. A table-change interrupt (new choices) → post the new block.
        let new_choices: Arc<[EnumEntry]> = Arc::from(vec![
            enum_entry("LOW", 0, 0),
            enum_entry("MID", 1, 1),
            enum_entry("HIGH", 2, 2),
        ]);
        ads.handle.interrupts().notify(InterruptValue {
            reason: ads.reason,
            addr: ads.addr,
            value: ParamValue::Enum {
                index: 0,
                choices: new_choices.clone(),
            },
            ..Default::default()
        });

        let post = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("property post must arrive within timeout")
            .expect("channel open");
        // C names `&pr->val` in all eight callbacks; the setEnums block below
        // is written, never posted on.
        assert_eq!(post.post_field, "VAL");
        let get = |name: &str| {
            post.writes
                .iter()
                .find(|(f, _)| f == name)
                .map(|(_, v)| v.clone())
        };
        // The delivered block is the NEW table (value-only #1 was suppressed).
        assert_eq!(get("ZRST"), Some(EpicsValue::String("LOW".into())));
        assert_eq!(get("ONST"), Some(EpicsValue::String("MID".into())));
        assert_eq!(get("TWST"), Some(EpicsValue::String("HIGH".into())));
        assert_eq!(get("TWVL"), Some(EpicsValue::ULong(2)));
        assert_eq!(get("TWSV"), Some(EpicsValue::Short(2)));

        // No further post is queued (the value-only interrupt produced none).
        assert!(
            rx.try_recv().is_err(),
            "value-only interrupt must not post a property update"
        );
    }

    /// The interface traits are asynInt32Base/asynInt64Base, whose getBounds
    /// is `*low = *high = 0` (asynInt32Base.c:99); a PortDriver is
    /// asynPortDriver, 0..65535 (asynPortDriver.cpp:2073-2074, :2212-2213).
    #[test]
    fn get_bounds_defaults_match_c() {
        use crate::interfaces::int32::AsynInt32;
        use crate::interfaces::int64::AsynInt64;

        struct Bare32;
        impl AsynInt32 for Bare32 {
            fn read_int32(&mut self, _u: &AsynUser) -> AsynResult<i32> {
                Ok(0)
            }
            fn write_int32(&mut self, _u: &mut AsynUser, _v: i32) -> AsynResult<()> {
                Ok(())
            }
        }
        struct Bare64;
        impl AsynInt64 for Bare64 {
            fn read_int64(&mut self, _u: &AsynUser) -> AsynResult<i64> {
                Ok(0)
            }
            fn write_int64(&mut self, _u: &mut AsynUser, _v: i64) -> AsynResult<()> {
                Ok(())
            }
        }

        let u = AsynUser::new(0);
        assert_eq!(Bare32.get_bounds(&u).unwrap(), (0, 0));
        assert_eq!(Bare64.get_bounds(&u).unwrap(), (0, 0));
        // PortDriver defaults — TestPort overrides neither.
        assert_eq!(TestPort::new().get_bounds_int32(&u).unwrap(), (0, 65535));
        assert_eq!(TestPort::new().get_bounds_int64(&u).unwrap(), (0, 65535));
    }

    // --- asyn:FIFO ring buffer ---

    fn intr_entry(v: i32, t_ms: u64) -> CachedInterrupt {
        CachedInterrupt {
            value: crate::param::ParamValue::Int32(v),
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(t_ms),
            alarm_status: 0,
            alarm_severity: 0,
            aux_status: crate::error::AsynStatus::Success,
            octet_nul: false,
        }
    }

    #[test]
    fn fifo_default_size_matches_c_constant() {
        // C devAsynInt32.c:63 → DEFAULT_RING_BUFFER_SIZE = 10.
        let f = InterruptFifo::new(tokio::sync::mpsc::channel(16).0);
        assert!(matches!(f.store, CallbackStore::Ring { size: 10, .. }));
        assert_eq!(f.waiting(), 0);
        assert_eq!(f.overflows, 0);
    }

    #[test]
    fn fifo_push_pop_fifo_order() {
        let mut f = InterruptFifo::new(tokio::sync::mpsc::channel(16).0);
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        let popped: Vec<_> = std::iter::from_fn(|| f.pop().map(|d| d.entry))
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![1, 2, 3], "FIFO order, not LIFO");
        assert_eq!(f.overflows, 0);
    }

    #[test]
    fn fifo_overflow_drops_oldest_and_counts() {
        // C parity: devAsynInt32.c:565-571 — when ringHead wraps onto
        // ringTail, advance ringTail (drop oldest) + overflows++.
        let mut f = InterruptFifo::new(tokio::sync::mpsc::channel(16).0);
        f.resize(3);
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        // Now full: every additional push must be reported as overflow.
        assert!(!f.push_with_overflow(intr_entry(4, 4)));
        assert!(!f.push_with_overflow(intr_entry(5, 5)));
        assert_eq!(f.overflows, 2);
        // Buffer now holds [3, 4, 5] — oldest two dropped.
        let popped: Vec<_> = std::iter::from_fn(|| f.pop().map(|d| d.entry))
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![3, 4, 5]);
    }

    #[test]
    fn fifo_pop_reports_and_clears_overflows() {
        let mut f = InterruptFifo::new(tokio::sync::mpsc::channel(16).0);
        f.resize(1);
        f.push_with_overflow(intr_entry(1, 1));
        f.push_with_overflow(intr_entry(2, 2)); // overflow
        f.push_with_overflow(intr_entry(3, 3)); // overflow
        assert_eq!(f.pop().map(|d| d.overflows), Some(2));
        f.push_with_overflow(intr_entry(4, 4));
        assert_eq!(f.pop().map(|d| d.overflows), Some(0), "the pop cleared it");
    }

    #[test]
    fn set_fifo_size_before_init_wins_over_the_dset_default() {
        use epics_base_rs::server::records::stringin::StringinRecord;
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.set_fifo_size(2);
        ads.init(&mut StringinRecord::new("")).unwrap();
        let g = ads.interrupt_fifo.lock().unwrap();
        assert!(matches!(g.store, CallbackStore::Ring { size: 2, .. }));
    }

    #[test]
    fn apply_record_info_parses_asyn_fifo_as_atoi() {
        let mut ads = make_adapter(ScanType::IoIntr);
        let long64 = std::ffi::c_long::BITS == 64;
        for (raw, tag) in [
            ("32", Some(32)),
            (" +7x", Some(7)),
            ("garbage", Some(0)),
            ("0", Some(0)),
            ("-3", Some(0)),
            // glibc's atoi is the saturated `long` cut to `int`: 1, then -1
            // which leaves the last, with a 64-bit `long`; INT_MAX twice with
            // a 32-bit one.
            (
                "4294967297",
                Some(if long64 { 1 } else { i32::MAX as usize }),
            ),
            (
                "99999999999999999999",
                Some(if long64 { 1 } else { i32::MAX as usize }),
            ),
        ] {
            let info =
                std::collections::HashMap::from([("asyn:FIFO".to_string(), raw.to_string())]);
            ads.apply_record_info(&info);
            assert_eq!(ads.fifo_tag, tag, "{raw:?}; a negative one leaves the last");
        }
    }

    /// Fire `values` at the record's parameter as one burst. The callbacks run
    /// inline in `notify`, so all of them land before the record reads.
    fn burst(ads: &AsynDeviceSupport, values: &[crate::param::ParamValue]) {
        let (reason, interrupts) = (ads.reason, ads.handle.interrupts());
        for value in values {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: value.clone(),
                timestamp: SystemTime::UNIX_EPOCH,
                uint32_changed_mask: u32::MAX,
                ..Default::default()
            });
        }
    }

    fn octets(texts: &[&str]) -> Vec<crate::param::ParamValue> {
        texts
            .iter()
            .map(|t| crate::param::ParamValue::Octet(t.as_bytes().into()))
            .collect()
    }

    fn int32s(values: impl IntoIterator<Item = i32>) -> Vec<crate::param::ParamValue> {
        values
            .into_iter()
            .map(crate::param::ParamValue::Int32)
            .collect()
    }

    /// The scanIoRequests waiting on the record's pump.
    fn wakes(rx: &mut tokio::sync::mpsc::Receiver<()>) -> usize {
        std::iter::from_fn(|| rx.try_recv().ok()).count()
    }

    // RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — ran and passed under
    // `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs`.
    /// devAsynOctet and devAsynXXXArray keep no ring by default
    /// (DEFAULT_RING_BUFFER_SIZE 0, devAsynOctet.c:73, devAsynXXXArray.cpp:43):
    /// each callback overwrites the record's value and asks for a process, so
    /// every process after a burst reads its last value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ring_0_reads_the_last_value_of_a_burst() {
        use epics_base_rs::server::records::{stringin::StringinRecord, waveform::WaveformRecord};
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();

        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a", "b", "c"]));
        assert_eq!(wakes(&mut rx), 3, "a scanIoRequest per callback");
        for _ in 0..3 {
            ads.read(&mut si).unwrap();
            assert_eq!(si.val(), Some(EpicsValue::String("c".into())));
        }
        assert!(
            !ads.callback_value_waiting(),
            "three processes, three callbacks"
        );

        let mut ads = make_adapter(ScanType::IoIntr);
        ads.set_iface_type("asynInt32Array");
        ads.bind_interrupt_gate(&gate);
        let mut wf = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut wf).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut wf).expect("io intr receiver");
        let arrays: Vec<_> = (1..=3)
            .map(|v| crate::param::ParamValue::Int32Array(vec![v; 2].into()))
            .collect();
        burst(&ads, &arrays);
        assert_eq!(wakes(&mut rx), 3, "a scanIoRequest per callback");
        ads.read(&mut wf).unwrap();
        assert_eq!(wf.val(), Some(EpicsValue::LongArray(vec![3, 3].into())));
    }

    /// A ring of N holds the N newest callbacks, oldest first, and asks for a
    /// process per fresh entry only: the scalar default of 10
    /// (devAsynInt32.c:63), and `asyn:FIFO` on an array.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ring_n_reads_the_n_newest_of_a_burst() {
        use epics_base_rs::server::records::{longin::LonginRecord, waveform::WaveformRecord};
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();

        let mut ads = make_adapter(ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s(1..=12));
        assert_eq!(wakes(&mut rx), 10, "no scanIoRequest on an overflow");
        let read: Vec<_> = (0..10)
            .map(|_| {
                ads.read(&mut li).unwrap();
                li.val()
            })
            .collect();
        let newest: Vec<_> = (3..=12).map(|v| Some(EpicsValue::Long(v))).collect();
        assert_eq!(read, newest);
        assert!(!ads.callback_value_waiting());

        let mut ads = make_adapter(ScanType::IoIntr);
        ads.set_iface_type("asynInt32Array");
        let info = std::collections::HashMap::from([("asyn:FIFO".to_string(), "3".to_string())]);
        ads.apply_record_info(&info);
        ads.bind_interrupt_gate(&gate);
        let mut wf = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut wf).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut wf).expect("io intr receiver");
        let arrays: Vec<_> = (1..=5)
            .map(|v| crate::param::ParamValue::Int32Array(vec![v].into()))
            .collect();
        burst(&ads, &arrays);
        assert_eq!(wakes(&mut rx), 3, "no scanIoRequest on an overflow");
        for v in 3..=5 {
            ads.read(&mut wf).unwrap();
            assert_eq!(wf.val(), Some(EpicsValue::LongArray(vec![v].into())));
        }
        assert!(!ads.callback_value_waiting());
    }

    /// `asyn:FIFO` is read with `atoi` (devAsynInt32.c:361-362,
    /// devAsynOctet.c:403-404): "2abc" rings 2 on a stringin, and "0" turns a
    /// longin's ring of 10 into the latest-value slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asyn_fifo_sizes_the_ring_as_atoi_reads_it() {
        use epics_base_rs::server::records::{longin::LonginRecord, stringin::StringinRecord};
        let gate = open_gate();
        let tag =
            |n: &str| std::collections::HashMap::from([("asyn:FIFO".to_string(), n.to_string())]);

        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.apply_record_info(&tag("2abc"));
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a", "b", "c", "d"]));
        assert_eq!(wakes(&mut rx), 2);
        for s in ["c", "d"] {
            ads.read(&mut si).unwrap();
            assert_eq!(si.val(), Some(EpicsValue::String(s.into())));
        }
        assert!(!ads.callback_value_waiting());

        let mut ads = make_adapter(ScanType::IoIntr);
        ads.apply_record_info(&tag("0"));
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s(1..=4));
        assert_eq!(wakes(&mut rx), 4);
        for _ in 0..4 {
            ads.read(&mut li).unwrap();
            assert_eq!(li.val(), Some(EpicsValue::Long(4)));
        }
        assert!(!ads.callback_value_waiting());
    }

    /// An `asyn:READBACK` octet output rings 1 by default (devAsynOctet.c:342):
    /// a burst asks for one readback, which reads the burst's last value.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_readback_octet_output_rings_one() {
        use epics_base_rs::server::records::stringout::StringoutRecord;
        let gate = open_gate();
        let mut ads = octet_param_adapter(b"", ScanType::Passive);
        ads.output = true;
        let info =
            std::collections::HashMap::from([("asyn:READBACK".to_string(), "1".to_string())]);
        ads.apply_record_info(&info);
        ads.bind_interrupt_gate(&gate);
        let mut so = StringoutRecord::new("");
        ads.init(&mut so).unwrap();
        let mut rx = ads.io_intr_receiver().expect("readback receiver");
        burst(&ads, &octets(&["a", "b", "c"]));
        assert_eq!(wakes(&mut rx), 1, "no request on an overflow");
        ads.read(&mut so).unwrap();
        assert_eq!(so.val(), Some(EpicsValue::String("c".into())));
        assert!(!ads.callback_value_waiting());
    }

    /// The latest-value slot replaces its value in place: past the first
    /// callback, a burst allocates nothing (the values are `Arc`s the driver
    /// built).
    #[test]
    fn a_ring_0_burst_allocates_nothing() {
        epics_libcom_rs::runtime::task::test_ioc_thread(|| {
            use crate::asyn_record::errs_alloc_tests::thread_allocs;
            use epics_base_rs::server::records::stringin::StringinRecord;
            let gate = open_gate();
            let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
            ads.bind_interrupt_gate(&gate);
            let mut si = StringinRecord::new("");
            ads.init(&mut si).unwrap();
            let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
            let values = octets(&["a", "b", "c", "d"]);
            burst(&ads, &values[..1]);
            let before = thread_allocs();
            burst(&ads, &values[1..]);
            let allocs = thread_allocs() - before;
            assert_eq!(allocs, 0, "a ring 0 callback allocates nothing");
            assert_eq!(wakes(&mut rx), 4);
        });
    }

    /// Routes the port's `mask` trace to a file, whose path it returns.
    fn trace_log(
        ads: &AsynDeviceSupport,
        mask: crate::trace::TraceMask,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("trace dir");
        let path = dir.path().join("warn.log");
        let file = std::fs::File::create(&path).unwrap();
        let (trace, port) = (ads.handle.user_trace().manager, ads.handle.port_name());
        trace.register_port(port, false);
        trace.set_trace_file(
            Some(port),
            crate::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some(port), mask);
        (dir, path)
    }

    fn lines_with(path: &std::path::Path, text: &str) -> usize {
        let log = std::fs::read_to_string(path).unwrap();
        log.lines().filter(|l| l.ends_with(text)).count()
    }

    /// The pop that drains an overflowed ring prints and clears C's count
    /// (devAsynInt32.c:775-780, devAsynOctet.c:463-468,
    /// devAsynXXXArray.cpp:383-388), the readback discard's pop included
    /// (devAsynOctet.c:587-589); a latest-slot process with callbacks still
    /// waiting warns (devAsynOctet.c:779-783, devAsynXXXArray.cpp:302-309).
    #[test]
    fn ring_pops_print_c_s_overflow_and_backlog_warnings() {
        use epics_base_rs::server::records::{
            bo::BoRecord, longin::LonginRecord, stringin::StringinRecord, waveform::WaveformRecord,
        };
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();
        let fifo =
            |n: &str| std::collections::HashMap::from([("asyn:FIFO".to_string(), n.to_string())]);

        let mut ads = make_adapter(ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let _rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s(1..=12));
        ads.read(&mut li).unwrap();
        ads.read(&mut li).unwrap();
        let overflow = "TEST:REC devAsynInt32::getCallbackValue warning, 2 ring buffer overflows";
        assert_eq!(lines_with(&log, overflow), 1, "printed once, then cleared");

        let mut ads = make_adapter(ScanType::Passive);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        let mut bo = BoRecord::new(0);
        ads.init(&mut bo).unwrap();
        ads.set_asyn_readback(true);
        for v in 1..=11 {
            let mut fifo = ads.interrupt_fifo.lock().unwrap();
            fifo.push_with_overflow(intr_entry(v, 0));
        }
        ads.arm_readback_callback();
        ads.reconcile_readback_callback();
        let overflow = "TEST:REC devAsynInt32::getCallbackValue warning, 1 ring buffer overflows";
        assert_eq!(lines_with(&log, overflow), 1, "the discard pops as C does");

        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        ads.apply_record_info(&fifo("2"));
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let _rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a", "b", "c", "d"]));
        ads.read(&mut si).unwrap();
        let overflow = "TEST:FW devAsynOctet::getRingBufferValue warning, 2 ring buffer overflows";
        assert_eq!(lines_with(&log, overflow), 1);

        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let _rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a", "b", "c"]));
        for _ in 0..3 {
            ads.read(&mut si).unwrap();
        }
        let backlog = "TEST:FW devAsynOctet::processCommon warning: multiple interrupt callbacks between processing";
        assert_eq!(
            lines_with(&log, backlog),
            2,
            "not on the process that drains it"
        );

        let arrays = |n: i32| -> Vec<_> {
            (1..=n)
                .map(|v| crate::param::ParamValue::Int32Array(vec![v].into()))
                .collect()
        };
        let mut ads = make_adapter(ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        ads.set_iface_type("asynInt32Array");
        ads.apply_record_info(&fifo("3"));
        ads.bind_interrupt_gate(&gate);
        let mut wf = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut wf).unwrap();
        let _rx = join_io_intr(&mut ads, &mut wf).expect("io intr receiver");
        burst(&ads, &arrays(5));
        ads.read(&mut wf).unwrap();
        let overflow =
            "TEST:REC devAsynXXXArray::getRingBufferValue  error, 2 ring buffer overflows";
        assert_eq!(lines_with(&log, overflow), 1);

        let mut ads = make_adapter(ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, crate::trace::TraceMask::WARNING);
        ads.set_iface_type("asynInt32Array");
        ads.bind_interrupt_gate(&gate);
        let mut wf = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut wf).unwrap();
        let _rx = join_io_intr(&mut ads, &mut wf).expect("io intr receiver");
        burst(&ads, &arrays(2));
        ads.read(&mut wf).unwrap();
        let backlog = "TEST:REC devAsynXXXArray::process, warning: multiple interrupt callbacks between processing";
        assert_eq!(lines_with(&log, backlog), 1);
    }

    /// C's scalar ring pop prints the value it took at ASYN_TRACEIO_DEVICE
    /// (devAsynInt32.c:782-785, devAsynFloat64.c:538-540, devAsynInt64.c:
    /// 611-613, devAsynUInt32Digital.c:618-620), the readback discard's pop
    /// included.
    #[test]
    fn a_scalar_ring_pop_prints_its_value() {
        use crate::param::ParamValue as V;
        use crate::trace::TraceMask;
        use epics_base_rs::server::records::{ai::AiRecord, bo::BoRecord, longin::LonginRecord};
        let gate = open_gate();
        let popped = |iface: &str, value: V, rec: &mut dyn Record| {
            let mut ads = if iface == "asynUInt32Digital" {
                make_uint32_io_intr_adapter(u32::MAX)
            } else {
                make_adapter(ScanType::IoIntr)
            };
            ads.set_iface_type(iface);
            let (dir, log) = trace_log(&ads, TraceMask::IO_DEVICE);
            ads.bind_interrupt_gate(&gate);
            ads.init(rec).unwrap();
            let _rx = join_io_intr(&mut ads, rec).expect("io intr receiver");
            burst(&ads, &[value]);
            ads.read(rec).unwrap();
            let text = std::fs::read_to_string(&log).unwrap();
            drop(dir);
            text
        };
        for (iface, value, line) in [
            (
                "asynInt32",
                V::Int32(-7),
                "TEST:REC devAsynInt32::getCallbackValue from ringBuffer value=-7",
            ),
            (
                "asynFloat64",
                V::Float64(2.5),
                "TEST:REC devAsynFloat64::getCallbackValue from ringBuffer value=2.500000",
            ),
            (
                "asynInt64",
                V::Int64(-5),
                "TEST:REC devAsynInt64::getCallbackValue from ringBuffer value=-5",
            ),
            (
                "asynUInt32Digital",
                V::UInt32Digital(u32::MAX),
                "TEST:U32 devAsynUInt32Digital::getCallbackValue from ringBuffer value=-1",
            ),
        ] {
            let text = if iface == "asynFloat64" {
                popped(iface, value, &mut AiRecord::new(0.0))
            } else {
                popped(iface, value, &mut LonginRecord::new(0))
            };
            assert!(
                text.lines().any(|l| l.ends_with(line)),
                "{iface}: missing {line:?} in\n{text}"
            );
        }

        let mut ads = make_adapter(ScanType::Passive);
        let (_dir, log) = trace_log(&ads, TraceMask::IO_DEVICE);
        let mut bo = BoRecord::new(0);
        ads.init(&mut bo).unwrap();
        ads.set_asyn_readback(true);
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(1, 0));
        ads.arm_readback_callback();
        ads.reconcile_readback_callback();
        let line = "TEST:REC devAsynInt32::getCallbackValue from ringBuffer value=1";
        assert_eq!(lines_with(&log, line), 1, "the discard pops as C does");
    }

    /// A callback's process dumps what the record then holds at
    /// ASYN_TRACEIO_DEVICE: devAsynOctet on every one (devAsynOctet.c:799-803),
    /// devAsynXXXArray only on a ring pop (devAsynXXXArray.cpp:321-324).
    #[test]
    fn a_callback_process_dumps_the_record_value() {
        use crate::trace::{TraceIoMask, TraceMask};
        use epics_base_rs::server::records::{stringin::StringinRecord, waveform::WaveformRecord};
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();

        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        let (_dir, log) = trace_log(&ads, TraceMask::IO_DEVICE);
        let trace = ads.handle.user_trace().manager;
        trace.set_trace_io_mask(Some(ads.handle.port_name()), TraceIoMask::ASCII);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let _rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["abc"]));
        ads.read(&mut si).unwrap();
        let dump = "TEST:FW devAsynOctet::processCommon len=3,  data:abc";
        assert_eq!(lines_with(&log, dump), 1);

        for (fifo, dumps) in [("2", 1), ("0", 0)] {
            let mut ads = make_adapter(ScanType::IoIntr);
            let (_dir, log) = trace_log(&ads, TraceMask::IO_DEVICE);
            let trace = ads.handle.user_trace().manager;
            trace.set_trace_io_mask(Some(ads.handle.port_name()), TraceIoMask::HEX);
            ads.set_iface_type("asynInt32Array");
            let info =
                std::collections::HashMap::from([("asyn:FIFO".to_string(), fifo.to_string())]);
            ads.apply_record_info(&info);
            ads.bind_interrupt_gate(&gate);
            let mut wf = WaveformRecord::new(4, DbFieldType::Long);
            ads.init(&mut wf).unwrap();
            let _rx = join_io_intr(&mut ads, &mut wf).expect("io intr receiver");
            burst(
                &ads,
                &[crate::param::ParamValue::Int32Array(vec![1, 2].into())],
            );
            ads.read(&mut wf).unwrap();
            let hex: String = [1i32, 2]
                .iter()
                .flat_map(|v| v.to_ne_bytes())
                .map(|b| format!("{b:02x} "))
                .collect();
            let dump = format!(
                "TEST:REC devAsynXXXArray::devAsynXXXArray nord=2, pRecord_->bptr data:\n{hex}\n"
            );
            let text = std::fs::read_to_string(&log).unwrap();
            assert_eq!(
                text.matches(&dump).count(),
                dumps,
                "asyn:FIFO {fifo}:\n{text}"
            );
        }
    }

    /// iocInit's I/O Intr pump: one process per doorbell until none is left.
    /// Returns what each process read.
    fn pump(
        ads: &mut AsynDeviceSupport,
        rx: &mut tokio::sync::mpsc::Receiver<()>,
        rec: &mut dyn Record,
    ) -> Vec<EpicsValue> {
        let mut read = Vec::new();
        while rx.try_recv().is_ok() {
            ads.arm_readback_callback();
            ads.read(rec).unwrap();
            ads.reconcile_readback_callback();
            read.extend(rec.get_field("VAL"));
            assert!(read.len() <= 100, "the doorbell never stops");
        }
        read
    }

    /// C scanIoRequests every fresh callback (devAsynOctet.c:506-519,
    /// devAsynInt32.c:564-576), one process each. The store owns that count;
    /// the 16-slot wake channel is a doorbell rung again after every pop
    /// while callbacks are owed, so a burst past 16 is still processed once
    /// per callback and leaves nothing owed.
    #[test]
    fn a_burst_is_processed_once_per_owed_callback() {
        use epics_base_rs::server::records::{longin::LonginRecord, stringin::StringinRecord};
        let gate = open_gate();
        let fifo =
            |n: &str| std::collections::HashMap::from([("asyn:FIFO".to_string(), n.to_string())]);

        for n in [1, 15, 16, 17, 40] {
            let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
            ads.bind_interrupt_gate(&gate);
            let mut si = StringinRecord::new("");
            ads.init(&mut si).unwrap();
            let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
            let texts: Vec<String> = (0..n).map(|i| format!("v{i}")).collect();
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            burst(&ads, &octets(&refs));
            let last = EpicsValue::String(format!("v{}", n - 1).into());
            assert_eq!(
                pump(&mut ads, &mut rx, &mut si),
                vec![last; n],
                "ring 0, burst {n}"
            );
            assert!(!ads.callback_value_waiting(), "ring 0, burst {n}");
            assert!(rx.try_recv().is_err(), "ring 0, burst {n}");
        }

        for (size, n, first) in [
            ("40", 1, 1),
            ("40", 15, 1),
            ("40", 16, 1),
            ("40", 17, 1),
            ("40", 40, 1),
            ("10", 40, 31),
        ] {
            let mut ads = make_adapter(ScanType::IoIntr);
            ads.apply_record_info(&fifo(size));
            ads.bind_interrupt_gate(&gate);
            let mut li = LonginRecord::new(0);
            ads.init(&mut li).unwrap();
            let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
            burst(&ads, &int32s(1..=n));
            let newest: Vec<_> = (first..=n).map(EpicsValue::Long).collect();
            assert_eq!(
                pump(&mut ads, &mut rx, &mut li),
                newest,
                "FIFO {size}, burst {n}"
            );
            assert!(!ads.callback_value_waiting(), "FIFO {size}, burst {n}");
        }
    }

    /// devAsynOctet's ringSize 0 keeps a callback that comes before
    /// `interruptAccept` (devAsynOctet.c:490-509), but its `scanIoRequest`
    /// does nothing then (dbScan.c:621): it is owed no process, and the first
    /// callback after the gate opens is processed once, taking the held ones
    /// with it.
    #[test]
    fn a_ring_0_callback_before_the_gate_owes_no_process() {
        use epics_base_rs::server::records::stringin::StringinRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        let gate = InterruptGate::new();
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a", "b", "c"]));
        assert!(rx.try_recv().is_err(), "closed: no request");
        assert!(ads.callback_value_waiting(), "closed: the value is unread");
        gate.set(true);
        burst(&ads, &octets(&["d"]));
        let d = EpicsValue::String("d".into());
        assert_eq!(pump(&mut ads, &mut rx, &mut si), vec![d]);
        assert!(!ads.callback_value_waiting());
    }

    /// PINI runs before `interruptAccept` (iocInit.c:196, dbScan.c:218) and
    /// takes a value a callback left before it, as C's `gotValue` test does
    /// (devAsynOctet.c:752), rather than reading the driver; the next process
    /// reads the driver.
    #[test]
    fn pini_takes_a_ring_0_value_held_before_the_gate() {
        use epics_base_rs::server::records::stringin::StringinRecord;
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        let gate = InterruptGate::new();
        let mut ads = octet_param_adapter(b"driver", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["held"]));
        ads.read(&mut si).unwrap();
        assert_eq!(si.val(), Some(EpicsValue::String("held".into())), "PINI");
        ads.read(&mut si).unwrap();
        assert_eq!(si.val(), Some(EpicsValue::String("driver".into())));
        assert!(rx.try_recv().is_err(), "nothing was owed a process");
    }

    /// Callbacks held before the gate count toward C's "multiple interrupt
    /// callbacks" warning on the take that clears them, and only that one.
    #[test]
    fn a_take_counts_held_callbacks_once() {
        let mut f = InterruptFifo::new(tokio::sync::mpsc::channel(16).0);
        f.resize(0);
        f.hold(intr_entry(1, 1));
        assert_eq!(f.pop().map(|d| d.backlog), Some(false), "one held");
        f.hold(intr_entry(2, 2));
        f.hold(intr_entry(3, 3));
        assert_eq!(f.pop().map(|d| d.backlog), Some(true), "two held");
        assert!(f.pop().is_none());
        f.hold(intr_entry(4, 4));
        f.push_with_overflow(intr_entry(5, 5));
        assert_eq!(f.pop().map(|d| d.backlog), Some(true), "held + owed");
        assert!(f.pop().is_none(), "C's gotValue stays one ahead here");
    }

    /// An overflow requests nothing (devAsynInt32.c:565-572), also while the
    /// pump holds a request it has not processed yet: requests 1 and 2, passes
    /// popping 2 and 3, and no third (a driver read, devAsynInt32.c:1045-1047).
    #[test]
    fn review_overflow_while_a_request_is_in_flight_requests_nothing() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::IoIntr);
        let info = std::collections::HashMap::from([("asyn:FIFO".to_string(), "2".to_string())]);
        ads.apply_record_info(&info);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s([1, 2]));
        assert!(rx.try_recv().is_ok(), "the scan thread takes request 1");
        burst(&ads, &int32s([3]));
        ads.arm_readback_callback();
        ads.read(&mut li).unwrap();
        ads.reconcile_readback_callback();
        let mut read = vec![li.get_field("VAL").unwrap()];
        read.extend(pump(&mut ads, &mut rx, &mut li));
        assert_eq!(read, vec![EpicsValue::Long(2), EpicsValue::Long(3)]);
    }

    /// A ring-0 callback landing while the pump holds a request is owed its
    /// own pass (devAsynOctet.c:518): the held pass and the next both read it.
    #[test]
    fn a_ring_0_callback_while_a_request_is_in_flight_is_processed() {
        use epics_base_rs::server::records::stringin::StringinRecord;
        let gate = open_gate();
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        burst(&ads, &octets(&["a"]));
        assert!(rx.try_recv().is_ok(), "the pump takes request a");
        burst(&ads, &octets(&["b"]));
        ads.arm_readback_callback();
        ads.read(&mut si).unwrap();
        ads.reconcile_readback_callback();
        let mut read = vec![si.get_field("VAL").unwrap()];
        read.extend(pump(&mut ads, &mut rx, &mut si));
        assert_eq!(read, vec![EpicsValue::String("b".into()); 2]);
    }

    /// A process outside the pump (PROC, a periodic scan) pops the entry a
    /// request was rung for; the request's pass then finds the ring empty and
    /// reads the driver, as C's does (devAsynInt32.c:836-841), and the next
    /// callback is requested again.
    #[test]
    fn a_pop_outside_the_pump_leaves_its_request_to_read_the_driver() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        // Before the record's interrupt client exists, so no callback lands.
        ads.handle.write_int32_blocking(ads.reason, 0, 5).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s([7]));
        ads.read(&mut li).unwrap();
        assert_eq!(li.get_field("VAL"), Some(EpicsValue::Long(7)), "PROC");
        assert_eq!(pump(&mut ads, &mut rx, &mut li), [EpicsValue::Long(5)]);
        // That pass answered its request though it popped nothing.
        burst(&ads, &int32s([8]));
        assert_eq!(pump(&mut ads, &mut rx, &mut li), [EpicsValue::Long(8)]);
    }

    /// A request the pump drops without a pass (record off I/O Intr, IOC
    /// paused) leaves its entry owed, and the next callback's requests reach
    /// both. C's dropped request is lost and its ring stays one process behind
    /// for good, as at a PACT bail (end_processing); not copied.
    #[test]
    fn a_request_dropped_without_a_pass_is_not_counted_after_a_pop() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s([1]));
        assert!(rx.try_recv().is_ok(), "the pump drops request 1");
        burst(&ads, &int32s([2]));
        assert_eq!(
            pump(&mut ads, &mut rx, &mut li),
            [1, 2].map(EpicsValue::Long)
        );
        assert!(!ads.callback_value_waiting());
    }

    /// Requests the pump drops without a pass (IOC paused) leave at most one
    /// counted as taken: after two drops a FIFO 2 overflow, which owes no
    /// new process, still rings for the two entries waiting.
    #[test]
    fn requests_dropped_while_paused_count_as_one_taken() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::IoIntr);
        let info = std::collections::HashMap::from([("asyn:FIFO".to_string(), "2".to_string())]);
        ads.apply_record_info(&info);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s([1, 2]));
        assert_eq!(wakes(&mut rx), 2, "the paused pump drops requests 1 and 2");
        burst(&ads, &int32s([3]));
        assert_eq!(
            pump(&mut ads, &mut rx, &mut li),
            [2, 3].map(EpicsValue::Long)
        );
    }

    /// A pass that bails on PACT (dbAccess.c:537) pops nothing and its
    /// request is spent: nothing rings again, so a held record does not spin
    /// its pump. The entry stays owed and the next callback rings for it; C
    /// leaves it with no request, one process behind for good, not copied.
    #[test]
    fn a_pass_that_pops_nothing_rings_nothing() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut li = LonginRecord::new(0);
        ads.init(&mut li).unwrap();
        let mut rx = join_io_intr(&mut ads, &mut li).expect("io intr receiver");
        burst(&ads, &int32s([7]));
        assert!(rx.try_recv().is_ok());
        ads.arm_readback_callback();
        ads.reconcile_readback_callback();
        assert!(rx.try_recv().is_err(), "the bail rings nothing");
        assert!(ads.callback_value_waiting());
        burst(&ads, &int32s([8]));
        let both = vec![EpicsValue::Long(7), EpicsValue::Long(8)];
        assert_eq!(pump(&mut ads, &mut rx, &mut li), both);
    }

    /// C resets `result.status` after a failed pass (devAsynOctet.c:817-821,
    /// devAsynXXXArray.cpp:335-339) but not the alarm it mapped into `result`
    /// (:806-809): a latest-slot entry's second pass alarms the same and
    /// returns 0, clearing UDF, until a new callback lands its own status.
    #[test]
    fn a_failed_latest_slot_entry_fails_its_first_pass_only() {
        use epics_base_rs::server::recgbl::alarm_status::TIMEOUT_ALARM;
        use epics_base_rs::server::records::stringin::StringinRecord;
        let gate = open_gate();
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        let _rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        let (reason, interrupts) = (ads.reason, ads.handle.interrupts().clone());
        let fire = |text: &str, aux_status| {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Octet(text.as_bytes().into()),
                timestamp: SystemTime::UNIX_EPOCH,
                aux_status,
                ..Default::default()
            });
        };
        let (ok, timeout) = (
            crate::error::AsynStatus::Success,
            crate::error::AsynStatus::Timeout,
        );
        let timed_out = Some((TIMEOUT_ALARM, 3));
        fire("a", ok);
        fire("b", timeout);
        fire("c", timeout);
        let mut pass = |ads: &mut AsynDeviceSupport| {
            let failed = ads.read(&mut si).unwrap().status.read_failed();
            (failed, ads.last_alarm().map(|a| (a.stat, a.sevr)))
        };
        assert_eq!(pass(&mut ads), (true, timed_out), "first pass");
        assert_eq!(pass(&mut ads), (false, timed_out), "status reset");
        assert_eq!(pass(&mut ads), (false, timed_out), "still reset");
        fire("d", timeout);
        assert_eq!(
            pass(&mut ads),
            (true, timed_out),
            "a new callback's own status"
        );
        assert_eq!(si.val(), Some(EpicsValue::String("a".into())));
    }

    /// LEN and NORD describe the value the pass posts. C sets them from the
    /// last callback's length, failed ones included (devAsynOctet.c:505), on a
    /// pass whose status is good (:776), and the reset after a failed pass
    /// (:817-820) makes the next one good: after "abcd" then a failed "x" it
    /// posts LEN 3 / NORD 2 over VAL "abcd". Not copied.
    #[test]
    fn a_latest_slot_len_describes_the_value_it_posts() {
        use epics_base_rs::server::records::{lsi::LsiRecord, waveform::WaveformRecord};
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();
        let records: [(Box<dyn Record>, &str, EpicsValue); 2] = [
            (Box::new(LsiRecord::new("zz")), "LEN", EpicsValue::ULong(6)),
            (
                Box::new(WaveformRecord::new(16, DbFieldType::Char)),
                "NORD",
                EpicsValue::ULong(5),
            ),
        ];
        for (mut rec, len, want) in records {
            let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
            ads.bind_interrupt_gate(&gate);
            ads.init(&mut *rec).unwrap();
            let _rx = join_io_intr(&mut ads, &mut *rec).expect("io intr receiver");
            let (reason, interrupts) = (ads.reason, ads.handle.interrupts().clone());
            // asynPortDriver's octet callback counts the NUL (strlen+1).
            for (text, aux_status) in [
                ("abcd", crate::error::AsynStatus::Success),
                ("x", crate::error::AsynStatus::Timeout),
            ] {
                interrupts.notify(InterruptValue {
                    reason,
                    addr: 0,
                    value: crate::param::ParamValue::Octet(text.as_bytes().into()),
                    timestamp: SystemTime::UNIX_EPOCH,
                    aux_status,
                    ..Default::default()
                });
            }
            for pass in ["failed", "reset"] {
                ads.read(&mut *rec).unwrap();
                let Some(EpicsValue::CharArray(val)) = rec.get_field("VAL") else {
                    panic!("{len}: VAL is a char array");
                };
                let text: Vec<u8> = val.iter().copied().take_while(|&c| c != 0).collect();
                assert_eq!(
                    (text.as_slice(), rec.get_field(len)),
                    (&b"abcd"[..], Some(want.clone())),
                    "{len}, {pass} pass"
                );
            }
        }
    }

    /// The reset holds with no good value ever copied: C's second pass still
    /// returns 0 (devAsynOctet.c:817-818) and the record keeps its value.
    #[test]
    fn a_latest_slot_with_no_good_value_resets_its_status_too() {
        use epics_base_rs::server::records::stringin::StringinRecord;
        let gate = open_gate();
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut si = StringinRecord::new("init");
        ads.init(&mut si).unwrap();
        let _rx = join_io_intr(&mut ads, &mut si).expect("io intr receiver");
        let (reason, interrupts) = (ads.reason, ads.handle.interrupts().clone());
        for _ in 0..2 {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Octet(b"x".as_slice().into()),
                aux_status: crate::error::AsynStatus::Timeout,
                ..Default::default()
            });
        }
        let failed: Vec<_> = (0..2)
            .map(|_| ads.read(&mut si).unwrap().status.read_failed())
            .collect();
        assert_eq!(failed, [true, false]);
        assert_eq!(si.val(), Some(EpicsValue::String("init".into())));
    }

    /// Callbacks that land while a queued write holds the record are owed,
    /// not rung (devAsynInt32.c:607-613); the cycle's end rings once, and the
    /// pops ring on until all are processed (:1013-1018).
    #[test]
    fn callbacks_owed_during_a_queued_write_ring_at_its_end() {
        use epics_base_rs::server::records::longout::LongoutRecord;
        let gate = open_gate();
        let mut ads = make_adapter(ScanType::Passive);
        ads.set_asyn_readback(true);
        ads.bind_interrupt_gate(&gate);
        let mut lo = LongoutRecord::new(0);
        ads.init(&mut lo).unwrap();
        let mut rx = ads.io_intr_receiver().expect("readback receiver");
        while rx.try_recv().is_ok() {}
        ads.interrupt_fifo.lock().unwrap().processing_async = true;
        burst(&ads, &int32s(1..=20));
        assert!(
            rx.try_recv().is_err(),
            "no process while the write holds it"
        );
        ads.end_processing();
        let owed: Vec<_> = (11..=20).map(EpicsValue::Long).collect();
        assert_eq!(pump(&mut ads, &mut rx, &mut lo), owed);
        assert!(!ads.callback_value_waiting());
    }

    #[test]
    fn compute_mask_shift_matches_c() {
        // C devAsynUInt32Digital.c:627-636 — position of lowest set bit.
        assert_eq!(compute_mask_shift(0x0001), 0);
        assert_eq!(compute_mask_shift(0x0002), 1);
        assert_eq!(compute_mask_shift(0x0080), 7);
        assert_eq!(compute_mask_shift(0x0F00), 8);
        assert_eq!(compute_mask_shift(0xFF00), 8);
        assert_eq!(compute_mask_shift(0x8000_0000), 31);
        // 0 mask is the "no bits" sentinel — C falls through to 32 too.
        assert_eq!(compute_mask_shift(0), 32);
    }

    /// C devAsynUInt32Digital.c:881 / 925 / 1010 / 1054 sets
    /// `pr->mask` and `pr->shft = computeShift(mask)` from the link
    /// mask so the record's RVAL→VAL conversion shifts the bits to
    /// bit-0. Verify the Rust adapter writes both fields at init.
    #[test]
    fn uint32_digital_init_propagates_mask_and_shft_to_record() {
        let mut ads = make_adapter(ScanType::Passive);
        ads.set_iface_type("asynUInt32Digital");
        ads = ads.with_mask(0xFF00);

        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut rec = MbbiRecord::default();
        ads.init(&mut rec).unwrap();

        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::ULong(0xFF00)),
            "MASK must propagate"
        );
        assert_eq!(
            rec.get_field("SHFT"),
            Some(EpicsValue::UShort(8)),
            "SHFT must equal computeShift(0xFF00) = 8"
        );
    }

    /// C init* sets `pr->mask = pPvt->mask` on every digital record, a 0 mask
    /// too, over the `.db`/NOBT mask (devAsynUInt32Digital.c:668, 710, 880,
    /// 924, 1009, 1053), and SHFT on the mbb ones (computeShift(0) = 32).
    #[test]
    fn uint32_digital_init_sets_every_records_mask_even_0() {
        use epics_base_rs::server::records::{
            bi::BiRecord, bo::BoRecord, mbbi::MbbiRecord, mbbi_direct::MbbiDirectRecord,
            mbbo::MbboRecord, mbbo_direct::MbboDirectRecord,
        };
        for mask in [0xFF00u32, 0] {
            let records: [(&str, Box<dyn Record>, bool); 6] = [
                ("bi", Box::new(BiRecord::new(0)), false),
                ("bo", Box::new(BoRecord::new(0)), false),
                ("mbbi", Box::new(MbbiRecord::new(0)), true),
                ("mbbo", Box::new(MbboRecord::new(0)), true),
                ("mbbiDirect", Box::new(MbbiDirectRecord::default()), true),
                ("mbboDirect", Box::new(MbboDirectRecord::default()), true),
            ];
            for (name, mut rec, has_shft) in records {
                rec.put_field("MASK", EpicsValue::ULong(0x0F)).unwrap();
                let mut ads = make_adapter(ScanType::Passive);
                ads.set_iface_type("asynUInt32Digital");
                ads = ads.with_mask(mask);
                ads.init(rec.as_mut()).unwrap();
                assert_eq!(
                    rec.get_field("MASK"),
                    Some(EpicsValue::ULong(mask)),
                    "{name} MASK, link mask {mask:#x}"
                );
                if has_shft {
                    assert_eq!(
                        rec.get_field("SHFT"),
                        Some(EpicsValue::UShort(compute_mask_shift(mask) as u16)),
                        "{name} SHFT, link mask {mask:#x}"
                    );
                }
            }
        }
    }

    /// C parity: devAsynOctet initCommon passes `plsi->sizv` as the
    /// asynOctet read-buffer size; an lsi record with a non-default
    /// SIZV must produce read ops sized accordingly.
    #[test]
    fn octet_buffer_picks_up_sizv_from_record() {
        let mut ads = make_adapter(ScanType::Passive);
        // The adapter only routes asynOctet through octet_max_size,
        // so re-target it.
        ads.set_iface_type("asynOctet");
        // Before init.
        assert_eq!(ads.octet_max_size, 256);

        use epics_base_rs::server::records::lsi::LsiRecord;
        let mut rec = LsiRecord::new("");
        // Default SIZV = 256 — still matches default.
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 256);

        // Bump SIZV and re-init: adapter must follow.
        rec.sizv = 1024;
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 1024);
    }

    /// A per-record octet cap from `drv_user_create` (DrvUserInfo.max_octet_len,
    /// the asyn-rs home for C `modbusDrvUser_t.len`) reduces `octet_max_size` to
    /// `min(buffer_len, cap)` at init, mirroring C `getStringLen`. The cap is
    /// applied AFTER SIZV finalizes the buffer, and a cap that exceeds the
    /// buffer leaves the length intact.
    #[test]
    fn octet_len_cap_from_drv_user_create_reduces_buffer() {
        // A port whose drv_user_create returns a fixed octet cap.
        struct CapPort {
            base: PortDriverBase,
            cap: usize,
        }
        impl PortDriver for CapPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn drv_user_create(
                &mut self,
                _req: &DrvUserRequest,
            ) -> crate::error::AsynResult<crate::port::DrvUserInfo> {
                Ok(crate::port::DrvUserInfo {
                    max_octet_len: Some(self.cap),
                    ..crate::port::DrvUserInfo::from_reason(0)
                })
            }
        }
        fn adapter_with_cap(cap: usize) -> AsynDeviceSupport {
            let mut base = PortDriverBase::new(
                "capport",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            let driver = CapPort { base, cap };
            let interrupts = Arc::new(InterruptManager::new());
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = PortActor::new(Box::new(driver), rx);
            let actor_link = actor.link();
            std::thread::Builder::new()
                .name("cap-actor".into())
                .spawn(actor.registered())
                .unwrap();
            let handle = PortHandle::new(
                tx,
                "capport".into(),
                interrupts,
                actor_link,
                std::sync::Arc::new(crate::trace::TraceManager::new()),
            );
            let link = AsynLink {
                port_name: "capport".into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "VAL".into(),
            };
            let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
            ads.set_record_info("TEST:CAP", ScanType::Passive);
            ads
        }

        use epics_base_rs::server::records::lsi::LsiRecord;

        // cap 5 < default buffer 256 → reduced to 5.
        let mut ads = adapter_with_cap(5);
        let mut rec = LsiRecord::new("");
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 5);

        // cap 512 applies to the SIZV-finalized buffer (1024) → 512, proving the
        // cap runs after SIZV.
        let mut ads = adapter_with_cap(512);
        let mut rec = LsiRecord::new("");
        rec.sizv = 1024;
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 512);

        // cap 4096 > buffer 256 → buffer unchanged (C `getStringLen` only caps
        // when drvUser->len < maxLen).
        let mut ads = adapter_with_cap(4096);
        let mut rec = LsiRecord::new("");
        ads.init(&mut rec).unwrap();
        assert_eq!(ads.octet_max_size, 256);
    }

    /// C `drvUserCreate` may hang a drvUser on the record's `pasynUser`
    /// (SPiiPlusDriver.cpp:306-322), which every later read and write of that
    /// record carries to the driver.
    #[test]
    fn the_drv_user_from_drv_user_create_rides_every_request() {
        use std::sync::Mutex;
        type Seen = Arc<Mutex<Vec<Option<String>>>>;
        struct ProgPort {
            base: PortDriverBase,
            seen: Seen,
        }
        impl ProgPort {
            fn see(&self, user: &AsynUser) {
                let prog = user
                    .drv_user
                    .as_ref()
                    .and_then(|d| d.downcast_ref::<String>())
                    .cloned();
                self.seen.lock().unwrap().push(prog);
            }
        }
        impl PortDriver for ProgPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn drv_user_create(
                &mut self,
                req: &DrvUserRequest,
            ) -> crate::error::AsynResult<crate::port::DrvUserInfo> {
                let name = req.drv_info.strip_prefix("START_").unwrap();
                Ok(crate::port::DrvUserInfo::from_reason(0)
                    .with_drv_user(Arc::new(name.to_string())))
            }
            fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
                self.see(user);
                self.base.write_int32_param(user, value)
            }
            fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
                self.see(user);
                self.base.read_int32_param(user)
            }
        }
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        use epics_base_rs::server::records::longout::LongoutRecord;

        let seen = Seen::default();
        let mut base = PortDriverBase::new(
            "progport",
            1,
            PortFlags::default(),
            crate::port::InterfaceMask::UNMIGRATED_ALL,
            crate::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("START_", ParamType::Int32).unwrap();
        let driver = ProgPort {
            base,
            seen: seen.clone(),
        };
        let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
        let link = AsynLink {
            port_name: "progport".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "START_PROG1".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynInt32");
        ads.set_record_info("TEST:PROG", ScanType::Passive);
        let mut rec = LongoutRecord::new(0);
        ads.init(&mut rec).unwrap();
        seen.lock().unwrap().clear();
        rec.set_val(EpicsValue::Long(1)).unwrap();
        ads.write(&mut rec).unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [Some("PROG1".to_string()), Some("PROG1".to_string())]
        );
    }

    // RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed under
    // `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs`.
    /// C's interrupt node keeps the record's `pasynUser`, so a driver walking
    /// its clients reads the address the link named and the drvUser
    /// `drvUserCreate` hung on it (drvModbusAsyn.cpp:1727-1733).
    #[tokio::test]
    async fn an_io_intr_client_carries_its_link_address_and_drv_user() {
        struct TagPort {
            base: PortDriverBase,
        }
        impl PortDriver for TagPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn drv_user_create(
                &mut self,
                req: &DrvUserRequest,
            ) -> crate::error::AsynResult<crate::port::DrvUserInfo> {
                Ok(crate::port::DrvUserInfo::from_reason(0)
                    .with_drv_user(Arc::new(req.drv_info.clone())))
            }
        }
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        let mut base = PortDriverBase::new(
            "tagport",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("DATA", ParamType::Int32).unwrap();
        let (rt, _jh) =
            create_port_runtime_unclaimed(TagPort { base }, RuntimeConfig::default()).unwrap();
        let link = AsynLink {
            port_name: "tagport".into(),
            addr: 3,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "INT16".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynInt32");
        ads.set_record_info("TEST:TAG", ScanType::IoIntr);
        let mut rec = epics_base_rs::server::records::ai::AiRecord::default();
        ads.init(&mut rec).unwrap();
        assert!(join_io_intr(&mut ads, &mut rec).is_some());
        let clients = rt.port_handle().interrupts().subscribed_clients();
        let [client] = clients.as_slice() else {
            panic!("{clients:?}");
        };
        assert_eq!(client.filter.user_addr, Some(3));
        let tag = client.filter.drv_user.as_ref().unwrap();
        assert_eq!(tag.downcast_ref::<String>().unwrap(), "INT16");
    }

    // RTEMS-EXEC-MODEL-ALLOW(20): the file-level marker's sites resume
    // here; checked as it says.
    /// R11-48. C device support calls `drvUser->create` only when the port
    /// registered asynDrvUser **and** the record named a userParam
    /// (`if (pasynInterface && pPvt->userParam)`, devAsynInt32.c:264-265). A byte
    /// transport registers no asynDrvUser: an `@asyn(L0,0,1)` link with no
    /// drvInfo binds at reason 0 and the record works. The port called
    /// `drv_user_create` unconditionally, took the transport's `ParamNotFound` as
    /// a bind failure, and returned from `init` before the buffer setup — leaving
    /// the record permanently dead (`reason_set == false` short-circuits read,
    /// write and the I/O Intr receiver).
    #[test]
    fn a_device_support_on_a_port_without_asyn_drv_user_binds_at_reason_zero() {
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
        use epics_base_rs::server::records::lsi::LsiRecord;

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
            fn read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                let bytes = b"HELLO";
                buf[..bytes.len()].copy_from_slice(bytes);
                crate::interpose::OctetReadResult::ok(
                    bytes.len(),
                    crate::interpose::EomReason::empty(),
                )
            }
        }

        let (rt, _jh) = create_port_runtime_unclaimed(
            Transport(PortDriverBase::new(
                "r11_48_dev",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        let link = AsynLink {
            port_name: "r11_48_dev".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            // `@asyn(L0,0,1)` with no drvInfo — the ordinary serial/IP link.
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynOctet");
        ads.set_record_info("TEST:R11_48", ScanType::Passive);

        let mut rec = LsiRecord::new("");
        rec.sizv = 1024;
        ads.init(&mut rec).unwrap();

        assert_eq!(ads.reason(), 0, "the transport's only reason");
        assert_eq!(
            ads.octet_max_size, 1024,
            "init ran to completion — the buffer setup below the drvUser branch is reached"
        );

        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::CharArray(b"HELLO\0".to_vec().into())),
            "the record is bound and reads from the transport"
        );
    }

    /// C devAsyn queues its output on the record's own `asynUser`, which
    /// `queueRequest` links onto the port queue without allocating
    /// (asynManager.c:1605-1628), and requests the record's `processCallback`
    /// it made at init (devAsynFloat64.c:351). A put on a blocking port queues
    /// and completes the same way: nothing past the first put allocates.
    #[test]
    fn a_put_on_a_blocking_port_queues_its_write_without_allocating() {
        epics_libcom_rs::runtime::task::test_ioc_thread(|| {
            use crate::asyn_record::errs_alloc_tests::thread_allocs;
            use crate::param::ParamType;
            use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
            use epics_base_rs::server::records::ao::AoRecord;

            struct Port(PortDriverBase);
            impl PortDriver for Port {
                fn base(&self) -> &PortDriverBase {
                    &self.0
                }
                fn base_mut(&mut self) -> &mut PortDriverBase {
                    &mut self.0
                }
            }

            let mut base = PortDriverBase::new(
                "WRITEALLOC",
                1,
                PortFlags {
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Float64).unwrap();
            let (rt, _jh) =
                create_port_runtime_unclaimed(Port(base), RuntimeConfig::default()).unwrap();
            let link = AsynLink {
                port_name: "WRITEALLOC".into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "VAL".into(),
            };
            let mut ads =
                AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynFloat64");
            ads.set_record_info("TEST:WRITEALLOC", ScanType::Passive);
            let mut rec = AoRecord::new(1.5);
            ads.init(&mut rec).unwrap();

            let db = epics_base_rs::server::database::PvDatabase::new();
            let cell = DeviceDone::new(
                &db,
                &epics_base_rs::server::record::RecordInstance::new(
                    "TEST:WRITEALLOC",
                    epics_base_rs::server::records::ao::AoRecord::new(0.0),
                ),
            );
            let source = DeviceDoneSource::new(&cell);
            let mut put = || {
                let before = thread_allocs();
                let start = ads.write_begin(&mut rec, &source).unwrap();
                let allocs = thread_allocs() - before;
                assert!(
                    matches!(start, WriteStart::Pending),
                    "a blocking port queues the write"
                );
                // Queued behind the write at its band: back once the write ran
                // and its cell is free for the next put.
                rt.port_handle()
                    .submit_blocking(
                        RequestOp::Float64Read,
                        AsynUser::new(0).with_priority(QueuePriority::Low),
                    )
                    .unwrap();
                allocs
            };
            assert_eq!(
                put(),
                0,
                "the record's cell is built at add_record, not by a put"
            );
            assert_eq!(put(), 0, "a put after the first allocates nothing");
        });
    }

    /// C `processAi` on a canBlock port queues the record's `asynUser` and
    /// returns with PACT set (devAsynInt32.c:836-841): a polled read on a
    /// blocking port queues through the cell made at init, without allocating.
    #[test]
    fn a_read_on_a_blocking_port_queues_without_allocating() {
        epics_libcom_rs::runtime::task::test_ioc_thread(|| {
            use crate::asyn_record::errs_alloc_tests::thread_allocs;
            use crate::param::ParamType;
            use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
            use epics_base_rs::server::records::ai::AiRecord;

            struct Port(PortDriverBase);
            impl PortDriver for Port {
                fn base(&self) -> &PortDriverBase {
                    &self.0
                }
                fn base_mut(&mut self) -> &mut PortDriverBase {
                    &mut self.0
                }
            }

            let mut base = PortDriverBase::new(
                "READALLOC",
                1,
                PortFlags {
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Float64).unwrap();
            let (rt, _jh) =
                create_port_runtime_unclaimed(Port(base), RuntimeConfig::default()).unwrap();
            let link = AsynLink {
                port_name: "READALLOC".into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "VAL".into(),
            };
            let mut ads =
                AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynFloat64");
            ads.set_record_info("TEST:READALLOC", ScanType::Passive);
            let mut rec = AiRecord::new(0.0);
            ads.init(&mut rec).unwrap();

            let db = epics_base_rs::server::database::PvDatabase::new();
            let cell = DeviceDone::new(
                &db,
                &epics_base_rs::server::record::RecordInstance::new(
                    "TEST:READALLOC",
                    epics_base_rs::server::records::ao::AoRecord::new(0.0),
                ),
            );
            let source = DeviceDoneSource::new(&cell);
            let mut read = || {
                let before = thread_allocs();
                let start = ads.read_begin(&mut rec, &source).unwrap();
                let allocs = thread_allocs() - before;
                assert!(
                    matches!(start, ReadStart::Pending),
                    "a blocking port queues the read"
                );
                // Queued behind the read at its band: back once it ran. The
                // parameter holds no value, so both reads fail; only the order
                // matters here.
                let _ = rt.port_handle().submit_blocking(
                    RequestOp::Float64Read,
                    AsynUser::new(0).with_priority(QueuePriority::Low),
                );
                allocs
            };
            assert_eq!(read(), 0, "the record's cell is built at init");
            assert_eq!(read(), 0, "a read after the first allocates nothing");
        });
    }

    /// An I/O Intr ai on a port that reads `7.0` and counts its reads, with the
    /// completion cell a `read_begin` arms.
    struct IntrRead {
        rt: crate::runtime::PortRuntimeHandle,
        ads: AsynDeviceSupport,
        rec: epics_base_rs::server::records::ai::AiRecord,
        reads: Arc<std::sync::atomic::AtomicUsize>,
        done: DeviceDone,
        _db: epics_base_rs::server::database::PvDatabase,
    }

    impl IntrRead {
        fn new(name: &str, can_block: bool) -> Self {
            use crate::param::ParamType;
            use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

            struct Port(PortDriverBase, Arc<std::sync::atomic::AtomicUsize>);
            impl PortDriver for Port {
                fn base(&self) -> &PortDriverBase {
                    &self.0
                }
                fn base_mut(&mut self) -> &mut PortDriverBase {
                    &mut self.0
                }
                fn read_float64(&mut self, _user: &AsynUser) -> AsynResult<f64> {
                    self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(7.0)
                }
            }

            let mut base = PortDriverBase::new(
                name,
                1,
                PortFlags {
                    can_block,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Float64).unwrap();
            let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (rt, _jh) =
                create_port_runtime_unclaimed(Port(base, reads.clone()), RuntimeConfig::default())
                    .unwrap();
            let link = AsynLink {
                port_name: name.into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "VAL".into(),
            };
            let mut ads =
                AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, "asynFloat64");
            ads.set_record_info(name, ScanType::IoIntr);
            let mut rec = epics_base_rs::server::records::ai::AiRecord::new(0.0);
            ads.init(&mut rec).unwrap();
            let db = epics_base_rs::server::database::PvDatabase::new();
            let done = DeviceDone::new(
                &db,
                &epics_base_rs::server::record::RecordInstance::new(
                    name,
                    epics_base_rs::server::records::ai::AiRecord::new(0.0),
                ),
            );
            Self {
                rt,
                ads,
                rec,
                reads,
                done,
                _db: db,
            }
        }

        fn begin(&mut self) -> ReadStart {
            self.ads
                .read_begin(&mut self.rec, &DeviceDoneSource::new(&self.done))
                .unwrap()
        }

        /// Back once a request queued at the record's band has run.
        fn port_ran(&self) {
            let _ = self.rt.port_handle().submit_blocking(
                RequestOp::Float64Read,
                AsynUser::new(0).with_priority(QueuePriority::Low),
            );
            self.reads.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn ring_len(&self) -> usize {
            self.ads.interrupt_fifo.lock().unwrap().waiting()
        }
    }

    /// C `getCallbackValue` found a value: the pass takes it and queues
    /// nothing, on either kind of port.
    #[test]
    fn a_ring_value_is_taken_without_a_read() {
        for (name, can_block) in [("INTRRINGBLK", true), ("INTRRINGSYN", false)] {
            let mut t = IntrRead::new(name, can_block);
            push_f64(&t.ads, 3.0);
            assert!(matches!(t.begin(), ReadStart::Synchronous), "{name}");
            t.ads.read(&mut t.rec).unwrap();
            assert_eq!(t.rec.val, 3.0, "{name}");
            assert_eq!(t.reads(), 0, "{name}: the device is not read");
        }
    }

    /// A ring entry left when SCAN moved off I/O Intr is still C
    /// `getCallbackValue`'s value (devAsynInt32.c:768-790, :836): a blocking
    /// port's pass takes it and queues no read.
    #[test]
    fn a_ring_entry_left_off_io_intr_queues_no_read() {
        use epics_base_rs::server::device_support::IoIntrCmd;
        let mut t = IntrRead::new("INTRLEFTBLK", true);
        let mut common = epics_base_rs::server::record::CommonFields::default();
        assert!(
            t.ads
                .get_ioint_info(&mut t.rec, &mut common, IoIntrCmd::Add)
        );
        push_f64(&t.ads, 3.0);
        assert!(
            t.ads
                .get_ioint_info(&mut t.rec, &mut common, IoIntrCmd::Delete)
        );
        assert!(matches!(t.begin(), ReadStart::Synchronous));
        t.ads.read(&mut t.rec).unwrap();
        assert_eq!(t.rec.val, 3.0);
        assert_eq!(t.reads(), 0, "the device is not read");
    }

    /// An empty ring is C's `queueRequest`: queued on a blocking port, read in
    /// process on a synchronous one.
    #[test]
    fn an_empty_ring_reads_the_device() {
        let mut t = IntrRead::new("INTREMPTYBLK", true);
        assert!(matches!(t.begin(), ReadStart::Pending));
        t.port_ran();
        assert_eq!(t.reads(), 1, "the port thread read");
        t.ads.read(&mut t.rec).unwrap();
        assert_eq!(t.rec.val, 7.0);

        let mut t = IntrRead::new("INTREMPTYSYN", false);
        assert!(matches!(t.begin(), ReadStart::Synchronous));
        t.ads.read(&mut t.rec).unwrap();
        assert_eq!(t.rec.val, 7.0);
        assert_eq!(t.reads(), 1, "read in process");
    }

    /// C pops the ring again on the pass the queued read re-enters
    /// (devAsynInt32.c:836): a value that arrived meanwhile replaces the read's.
    #[test]
    fn a_callback_before_the_reentered_pass_replaces_the_read() {
        let mut t = IntrRead::new("INTRRACE", true);
        assert!(matches!(t.begin(), ReadStart::Pending));
        t.port_ran();
        push_f64(&t.ads, 9.0);
        t.ads.read(&mut t.rec).unwrap();
        assert_eq!(t.rec.val, 9.0);
        assert!(
            t.ads.device_job.read.lock().is_none(),
            "the read's result does not reach the next pass"
        );
    }

    /// A refused `queueRequest` is the pass whose ring C already found empty:
    /// it applies the refusal and leaves a value that arrived since for the
    /// next pass.
    #[test]
    fn a_refused_read_leaves_a_later_callback_in_the_ring() {
        let mut t = IntrRead::new("INTRREFUSED", true);
        t.rt.port_handle().set_enable(-1, false).unwrap();
        assert!(matches!(t.begin(), ReadStart::Synchronous));
        push_f64(&t.ads, 9.0);
        t.ads.read(&mut t.rec).unwrap();
        assert_eq!(t.rec.val, 0.0);
        assert_eq!(
            t.ads.last_alarm_severity,
            epics_base_rs::server::record::AlarmSeverity::Invalid as u16
        );
        assert_eq!(t.ring_len(), 1);
    }

    /// C writes a command, an octet waveform or an array from the record's
    /// own buffer (devAsynOctet.c:853, :1071; devAsynXXXArray.cpp:295): a
    /// request that carries data refills the last one's buffer, so only the
    /// first process allocates it.
    #[test]
    fn a_request_that_carries_data_refills_the_last_one_s_buffer() {
        epics_libcom_rs::runtime::task::test_ioc_thread(|| {
            use crate::asyn_record::errs_alloc_tests::thread_allocs;
            use crate::param::ParamType;
            use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
            use epics_base_rs::server::records::stringin::StringinRecord;
            use epics_base_rs::server::records::waveform::WaveformRecord;
            use epics_base_rs::types::DbFieldType;

            struct Port(PortDriverBase);
            impl PortDriver for Port {
                fn base(&self) -> &PortDriverBase {
                    &self.0
                }
                fn base_mut(&mut self) -> &mut PortDriverBase {
                    &mut self.0
                }
            }

            let mut base = PortDriverBase::new(
                "BUFREUSE",
                1,
                PortFlags {
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            let msg = base.create_param("MSG", ParamType::Octet).unwrap();
            let arr = base.create_param("ARR", ParamType::Int32Array).unwrap();
            let (rt, _jh) =
                create_port_runtime_unclaimed(Port(base), RuntimeConfig::default()).unwrap();
            let adapter = |iface: &str, reason: usize| {
                let link = AsynLink {
                    port_name: "BUFREUSE".into(),
                    addr: 0,
                    timeout: Some(Duration::from_secs(1)),
                    drv_info: String::new(),
                };
                let mut ads = AsynDeviceSupport::from_handle(rt.port_handle().clone(), link, iface);
                ads.set_record_info("TEST:BUFREUSE", ScanType::Passive);
                ads.reason = reason;
                ads.reason_set = true;
                ads.request_cell();
                ads
            };

            let db = epics_base_rs::server::database::PvDatabase::new();
            let cell = DeviceDone::new(
                &db,
                &epics_base_rs::server::record::RecordInstance::new(
                    "TEST:BUFREUSE",
                    epics_base_rs::server::records::ao::AoRecord::new(0.0),
                ),
            );
            let source = DeviceDoneSource::new(&cell);
            let allocs = |ads: &mut AsynDeviceSupport, rec: &mut dyn Record| {
                (0..3)
                    .map(|_| {
                        let before = thread_allocs();
                        let start = ads.read_begin(rec, &source).unwrap();
                        let allocs = thread_allocs() - before;
                        assert!(matches!(start, ReadStart::Pending));
                        // Queued behind the request: back once it ran.
                        let _ = rt.port_handle().submit_blocking(
                            RequestOp::Float64Read,
                            AsynUser::new(0).with_priority(QueuePriority::Low),
                        );
                        allocs
                    })
                    .collect::<Vec<_>>()
            };

            let mut cmd = adapter("asynOctet", msg);
            cmd.octet_cmd = Some(b"PING\r".to_vec());
            let counts = allocs(&mut cmd, &mut StringinRecord::new(""));
            assert_eq!(counts[1..], [0, 0], "asynOctetCmdResponse: {counts:?}");

            let mut octet = adapter("asynOctet", msg);
            octet.write_only = true;
            let mut wf = WaveformRecord::new(16, DbFieldType::Char);
            wf.put_field("VAL", EpicsValue::CharArray(b"abc".to_vec().into()))
                .unwrap();
            let counts = allocs(&mut octet, &mut wf);
            assert_eq!(counts[1..], [0, 0], "asynOctetWrite: {counts:?}");

            let mut array = adapter("asynInt32Array", arr);
            array.write_only = true;
            let mut wf = WaveformRecord::new(4, DbFieldType::Long);
            wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3].into()))
                .unwrap();
            let counts = allocs(&mut array, &mut wf);
            assert_eq!(counts[1..], [0, 0], "asynInt32ArrayOut: {counts:?}");
        });
    }

    /// R11-48 negative control: on a port that DOES register asynDrvUser, an
    /// unresolvable drvInfo is a bind failure — C prints and `goto bad`
    /// (devAsynInt32.c:272-276), whose body is `recGblSetSevr(pr, LINK_ALARM,
    /// INVALID_ALARM); pr->pact = 1` (:348-351). Unbound is not enough: reason 0
    /// is a real parameter, so a record left processing would publish another
    /// record's value as its own.
    #[test]
    fn a_device_support_with_an_unknown_drv_info_still_fails_to_bind() {
        use epics_base_rs::server::record::AlarmSeverity;
        use epics_base_rs::server::records::longin::LonginRecord;

        let mut ads = make_adapter(ScanType::Passive);
        ads.drv_info = "NO_SUCH_PARAM".to_string();

        let mut rec = LonginRecord::default();
        assert_eq!(
            ads.init(&mut rec).unwrap(),
            DeviceInitOutcome::dead_with_alarm(
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                AlarmSeverity::Invalid
            ),
            "C `goto bad` — LINK_ALARM/INVALID, then the record is dead"
        );

        assert!(
            !ads.reason_set,
            "a param-library port that cannot resolve the drvInfo leaves the record unbound"
        );
    }

    /// A record's bind carries the asyn interface it will read the parameter
    /// through (from its DTYP), so an on-demand driver creates the parameter
    /// with that type instead of guessing.
    ///
    /// C parity: `adsAsynPortDriver::getRecordInfoFromDrvInfo` derives each
    /// parameter's asyn type from the bound record's DTYP — the same PLC symbol
    /// may bind as `asynInt32` from one record and `asynFloat64` from another.
    /// Without the interface at bind, a lazily-created parameter can only be
    /// guessed from the drvInfo string, and a wrong guess feeds the record a
    /// value of the wrong `ParamValue` variant on every I/O Intr callback.
    #[test]
    fn drv_user_create_receives_the_records_interface() {
        /// What the driver saw and what it therefore created, per bind.
        type Binds = Arc<std::sync::Mutex<Vec<(String, Option<InterfaceType>, ParamType)>>>;

        /// An on-demand driver (C Autoparam lazy creation): it holds no
        /// parameters up front and creates one per drvInfo as records bind,
        /// typed by the interface the record announced.
        struct OnDemandPort {
            base: PortDriverBase,
            binds: Binds,
        }
        impl PortDriver for OnDemandPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn drv_user_create(
                &mut self,
                req: &DrvUserRequest,
            ) -> crate::error::AsynResult<crate::port::DrvUserInfo> {
                let param_type = match req.iface {
                    Some(InterfaceType::Float64) => ParamType::Float64,
                    Some(InterfaceType::Int32) => ParamType::Int32,
                    Some(InterfaceType::Octet) => ParamType::Octet,
                    // No interface announced: the driver has nothing to honour.
                    _ => ParamType::Int32,
                };
                self.binds
                    .lock()
                    .unwrap()
                    .push((req.drv_info.clone(), req.iface, param_type));
                // A SyncIO connect creates the same drvInfo again: find it.
                let reason = match self.base().find_param(&req.drv_info) {
                    Some(reason) => reason,
                    None => self.base_mut().create_param(&req.drv_info, param_type)?,
                };
                Ok(crate::port::DrvUserInfo::from_reason(reason))
            }
        }

        /// Bind one record on `iface_type` to the drvInfo `SYMBOL` and return
        /// what the driver was told at bind.
        fn bind(iface_type: &str, init: impl FnOnce(&mut AsynDeviceSupport)) -> Binds {
            let binds: Binds = Arc::new(std::sync::Mutex::new(Vec::new()));
            let driver = OnDemandPort {
                base: PortDriverBase::new(
                    "ondemand",
                    1,
                    PortFlags::default(),
                    InterfaceMask::UNMIGRATED_ALL,
                    InterfaceMask::UNMIGRATED_INTERRUPTS,
                ),
                binds: Arc::clone(&binds),
            };
            let interrupts = Arc::new(InterruptManager::new());
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = PortActor::new(Box::new(driver), rx);
            let actor_link = actor.link();
            std::thread::Builder::new()
                .name("ondemand-actor".into())
                .spawn(actor.registered())
                .unwrap();
            let handle = PortHandle::new(
                tx,
                "ondemand".into(),
                interrupts,
                actor_link,
                std::sync::Arc::new(crate::trace::TraceManager::new()),
            );
            let link = AsynLink {
                port_name: "ondemand".into(),
                addr: 0,
                timeout: Some(Duration::from_secs(1)),
                drv_info: "SYMBOL".into(),
            };
            let mut ads = AsynDeviceSupport::from_handle(handle, link, iface_type);
            ads.set_record_info("TEST:SYMBOL", ScanType::Passive);
            init(&mut ads);
            binds
        }

        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::longin::LonginRecord;

        // The same drvInfo bound by an `ai` with DTYP=asynFloat64: the driver
        // must be told Float64 and create a Float64 parameter, on initCommon's
        // create and again on its Float64SyncIO connect.
        let binds = bind("asynFloat64", |ads| {
            let mut rec = AiRecord::new(0.0);
            ads.init(&mut rec).unwrap();
        });
        let float64 = (
            "SYMBOL".to_string(),
            Some(InterfaceType::Float64),
            ParamType::Float64,
        );
        assert_eq!(binds.lock().unwrap().as_slice(), [float64.clone(), float64]);

        // The same drvInfo bound by a `longin` with DTYP=asynInt32: Int32, on
        // initCommon's create and again on its Int32SyncIO connect.
        let binds = bind("asynInt32", |ads| {
            let mut rec = LonginRecord::new(0);
            ads.init(&mut rec).unwrap();
        });
        let int32 = (
            "SYMBOL".to_string(),
            Some(InterfaceType::Int32),
            ParamType::Int32,
        );
        assert_eq!(binds.lock().unwrap().as_slice(), [int32.clone(), int32]);
    }

    /// devBusyAsyn.c:245-260 — the busy module's device support registers the
    /// output interrupt callback unconditionally, no `asyn:READBACK` info
    /// gate. A Passive busy record with no info tags must therefore come out
    /// of `init` with the readback path active (that is how the driver's
    /// param write releases the record), and an explicit
    /// `info(asyn:READBACK, "0")` must not defeat it (C never consults it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_record_enables_readback_unconditionally() {
        use epics_base_rs::server::records::busy::BusyRecord;
        let mut ads = make_adapter(ScanType::Passive);
        let mut info = std::collections::HashMap::new();
        info.insert("asyn:READBACK".to_string(), "0".to_string());
        ads.apply_record_info(&info);
        let mut rec = BusyRecord::new();
        ads.init(&mut rec).unwrap();
        assert!(
            ads.io_intr_receiver().is_some(),
            "busy must register the driver readback with no info tag"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_enables_readback_for_truthy_value() {
        // info("asyn:READBACK", "1") on a Passive output must allow
        // io_intr_receiver to activate (asyn upstream PRs #60 / #208).
        let mut ads = make_adapter(ScanType::Passive);
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        // Without the info tag, Passive scan keeps io_intr_receiver=None.
        assert!(ads.io_intr_receiver().is_none());
        // Apply the tag — adapter should now expose an Intr receiver.
        let mut info = std::collections::HashMap::new();
        info.insert("asyn:READBACK".to_string(), "1".to_string());
        ads.apply_record_info(&info);
        assert!(
            ads.io_intr_receiver().is_some(),
            "asyn:READBACK=1 must enable readback path"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_falsey_values_do_not_enable_readback() {
        let mut ads = make_adapter(ScanType::Passive);
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        for falsey in ["0", "no", "NO", "false", "False", ""] {
            let mut info = std::collections::HashMap::new();
            info.insert("asyn:READBACK".to_string(), falsey.to_string());
            ads.apply_record_info(&info);
            assert!(
                ads.io_intr_receiver().is_none(),
                "value {falsey:?} must not enable readback"
            );
        }
    }

    /// C parity for `asynDbGetInfo(precord, "asyn:INITIAL_READBACK")`
    /// at devAsynOctet.c:357 — info tag overrides an octet output's
    /// default. Verifies the auto-parse path: the framework calls
    /// `apply_record_info`, which must flip `initial_readback`
    /// without the caller having to invoke `set_initial_readback`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_record_info_handles_initial_readback_tag() {
        let mut ads = make_adapter(ScanType::Passive);
        // Starts off (default for adapters built with from_handle).
        assert!(!ads.initial_readback);

        let mut info = std::collections::HashMap::new();
        info.insert("asyn:INITIAL_READBACK".to_string(), "1".to_string());
        // Only devAsynOctet's output reads the tag.
        ads.apply_record_info(&info);
        assert!(!ads.initial_readback, "a non-octet-output dset ignores it");
        (ads.output, ads.dset) = (true, Dset::Octet);
        ads.apply_record_info(&info);
        assert!(ads.initial_readback, "info tag must enable readback");

        // Falsey value resets it.
        info.insert("asyn:INITIAL_READBACK".to_string(), "0".to_string());
        ads.apply_record_info(&info);
        assert!(!ads.initial_readback, "value '0' must disable readback");
    }

    #[test]
    fn test_adapter_write_read() {
        let mut ads = make_adapter(ScanType::Passive);

        use epics_base_rs::server::records::longin::LonginRecord;
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();

        // Write a value
        rec.set_val(EpicsValue::Long(42)).unwrap();
        ads.write(&mut rec).unwrap();

        // Read it back
        let mut rec2 = LonginRecord::new(0);
        ads.read(&mut rec2).unwrap();
        assert_eq!(rec2.val(), Some(EpicsValue::Long(42)));
    }

    #[test]
    fn io_intr_read_applies_cached_driver_alarm() {
        // C devAsynInt32.c:561-563/843-847 — the I/O-Intr ring element
        // carries the driver alarm and processXxx recGblSetSevr's it.
        // The CachedInterrupt previously dropped it, leaving I/O-Intr
        // records permanently NO_ALARM.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap(); // resolves reason_set
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(7),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 3,   // e.g. READ_ALARM
                alarm_severity: 1, // e.g. MINOR
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        }
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(7)));
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((3, 1)),
            "IoIntr read must surface the driver alarm carried by the ring entry"
        );
    }

    /// C `processAi` pops the ring before it asks the driver, whatever SCAN
    /// the record is in (devAsynInt32.c:836): an entry queued before SCAN
    /// moved off I/O Intr is the next periodic process's value.
    #[test]
    fn a_ring_entry_left_by_a_scan_move_is_read() {
        use epics_base_rs::server::device_support::IoIntrCmd;
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::Passive);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let mut common = epics_base_rs::server::record::CommonFields::default();
        assert!(ads.get_ioint_info(&mut rec, &mut common, IoIntrCmd::Add));
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(9, 0));
        assert!(ads.get_ioint_info(&mut rec, &mut common, IoIntrCmd::Delete));
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(9)));
        assert!(ads.interrupt_fifo.lock().unwrap().pop().is_none());
    }

    #[test]
    fn io_intr_read_no_alarm_when_entry_clean() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap(); // resolves reason_set
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.push_with_overflow(intr_entry(9, 0)); // alarm 0/0
        }
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(9)));
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            None,
            "clean entry => no alarm"
        );
    }

    /// A transport-error I/O Intr sample (`aux_status != Success`): C `processAi`
    /// maps the status via `asynStatusToEpicsAlarm` and returns -1, DISCARDING
    /// the value so RVAL/VAL keep their prior content (devAsynInt32.c:844-855).
    /// The port dropped `iv.aux_status`, storing the bad value with no alarm.
    #[test]
    fn io_intr_read_transport_error_discards_value_and_maps_timeout() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // asynInt32 input
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        // Seed a known prior value with a clean interrupt.
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(intr_entry(11, 0));
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(11)));
        assert_eq!(ads.last_alarm().map(|a| (a.stat, a.sevr)), None);
        // Now a transport-error interrupt carrying a different value.
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(99),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Timeout,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(11)),
            "transport error must keep the prior value (C return -1), not store 99"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((10, 3)),
            "transport Timeout maps to TIMEOUT_ALARM(10)/INVALID(3)"
        );
    }

    /// The `asynError`/unknown transport status takes C's direction-dependent
    /// `defaultStat`: READ_ALARM for an input I/O Intr record (processAi,
    /// devAsynInt32.c:844).
    #[test]
    fn io_intr_read_input_error_default_is_read_alarm() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // input, asyn_readback=false
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(99),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Error,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((1, 3)),
            "input asynError default => READ_ALARM(1)/INVALID(3)"
        );
    }

    /// Contrast with the input case: a scalar `asyn:READBACK` OUTPUT record takes
    /// WRITE_ALARM for the `asynError`/unknown default (processBo, devAsynInt32.c:
    /// 1219). Same shared ring read, direction picked from `asyn_readback`.
    #[test]
    fn io_intr_readback_output_error_default_is_write_alarm() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::Passive); // scalar asynInt32
        ads.set_asyn_readback(true); // output readback path
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(99),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Error,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((2, 3)),
            "scalar output readback asynError default => WRITE_ALARM(2)/INVALID(3)"
        );
    }

    /// C `asynStatusToEpicsAlarm` fills a field only while it is still NO_ALARM
    /// (asynEpicsUtils.c:242-264), so a sample's own EPICS alarm wins over the
    /// transport-status mapping.
    #[test]
    fn io_intr_read_sample_epics_alarm_wins_over_transport_mapping() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(99),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 7,   // STATE_ALARM (sample's own)
                alarm_severity: 2, // MAJOR
                aux_status: crate::error::AsynStatus::Timeout, // would map to (10, 3)
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((7, 2)),
            "sample STATE_ALARM/MAJOR must win over the transport Timeout mapping"
        );
    }

    /// The discard skips only the value store, NEVER the entry consume: C resets
    /// the ring tail on every `getCallbackValue` regardless of status. A
    /// transport-error entry is popped, and the next clean entry is the value
    /// stored — the ring stays balanced.
    #[test]
    fn io_intr_read_transport_error_consumes_entry_keeps_ring_balanced() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(99),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Disconnected,
                octet_nul: false,
            });
            g.push_with_overflow(intr_entry(50, 0)); // clean follow-up
        }
        // First read: error entry consumed (popped), value discarded.
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(0)),
            "error value discarded"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((9, 3)),
            "Disconnected maps to COMM_ALARM(9)/INVALID(3)"
        );
        assert_eq!(
            ads.interrupt_fifo.lock().unwrap().waiting(),
            1,
            "the error entry must be consumed, leaving exactly the clean follow-up"
        );
        // Second read: the clean entry is the value stored.
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(50)));
        assert_eq!(ads.last_alarm().map(|a| (a.stat, a.sevr)), None);
    }

    /// A driver that draws into the read buffer: it marks element 0 and
    /// keeps the rest, as a C driver drawing into `bptr` does.
    struct DrawPort {
        base: PortDriverBase,
    }
    impl PortDriver for DrawPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn read_int32_array(&mut self, _user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
            buf[0] = 7;
            Ok(3)
        }
        fn read_int8_array(&mut self, _user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize> {
            buf[0] = -1;
            Ok(3)
        }
    }

    fn draw_port_adapter(iface: &str, param: ParamType) -> AsynDeviceSupport {
        let mut base = PortDriverBase::new(
            "draw",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", param).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(DrawPort { base }), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-draw-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "draw".into(),
            Arc::new(InterruptManager::new()),
            actor_link,
            Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "draw".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.set_record_info("TEST:DRAW", ScanType::Passive);
        ads
    }

    /// C reads into the record's own buffer (devAsynXXXArray.cpp:352), so a
    /// driver drawing into it sees what a caput left there.
    #[test]
    fn an_array_read_hands_the_driver_the_records_contents() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = draw_port_adapter("asynInt32Array", ParamType::Int32Array);
        let mut rec = WaveformRecord::new(4, DbFieldType::Long);
        ads.init(&mut rec).unwrap();
        rec.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3].into()))
            .unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::LongArray(vec![7, 2, 3].into())));
    }

    /// The same through a cast: a UCHAR waveform on asynInt8Array.
    #[test]
    fn an_int8_array_read_hands_the_driver_the_records_bytes() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = draw_port_adapter("asynInt8Array", ParamType::Int8Array);
        let mut rec = WaveformRecord::new(4, DbFieldType::Char);
        ads.init(&mut rec).unwrap();
        rec.put_field("VAL", EpicsValue::CharArray(vec![1, 2, 3].into()))
            .unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::CharArray(vec![255, 2, 3].into()))
        );
    }

    /// Array dsets gate the same way (devAsynXXXArray.cpp:317 copies bptr/nord
    /// only on `rp->status == asynSuccess`) but use READ_ALARM as the default
    /// EVEN for aao output (the single shared process(), :330-331). So an array
    /// `asyn:READBACK` output discards the array on error and raises READ_ALARM,
    /// not WRITE_ALARM.
    #[test]
    fn io_intr_array_transport_error_discards_array_and_maps_read_alarm() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        // Build an asynInt32Array adapter on the readback (output) path.
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-arr-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32Array");
        ads.set_record_info("TEST:ARR", ScanType::Passive);
        ads.set_asyn_readback(true); // array OUTPUT readback (aao)
        let mut rec = WaveformRecord::new(8, DbFieldType::Long);
        ads.init(&mut rec).unwrap();

        // Seed a known prior array via a clean interrupt.
        let clean = std::sync::Arc::from([1i32, 2, 3].as_slice());
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32Array(clean),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::LongArray(vec![1, 2, 3].into())));

        // A transport-error array interrupt carrying different data.
        let bad = std::sync::Arc::from([9i32, 9, 9].as_slice());
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32Array(bad),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Error,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::LongArray(vec![1, 2, 3].into())),
            "transport error must keep the prior array (C devAsynXXXArray.cpp:317)"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((1, 3)),
            "array dset uses READ_ALARM(1) even for aao output, not WRITE_ALARM"
        );
    }

    /// Polled (non-interrupt) read with a non-success device status must DISCARD
    /// the value and keep the record's prior value, mirroring C processAi:
    /// `if (result.status == asynSuccess) { pr->rval = value } else { return -1 }`
    /// (devAsynInt32.c:848-855). The alarm is still raised (mapped + recGblSetSevr
    /// before the gate, :844-847). This is the polled-read sibling of the I/O
    /// Intr ring's aux_status store gate.
    /// An asynOctet adapter on an asynPortDriver string parameter holding
    /// `text`.
    fn octet_param_adapter(text: &[u8], scan: ScanType) -> AsynDeviceSupport {
        let mut base = PortDriverBase::new(
            "octparam",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let fw = base.create_param("FW", ParamType::Octet).unwrap();
        base.params.set_string(fw, 0, text).unwrap();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(TestPort { base }), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-octparam-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "octparam".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "octparam".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "FW".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.set_record_info("TEST:FW", scan);
        ads
    }

    /// C readOctet counts the NUL (`strlen + 1`) and callbackWfRead sets
    /// `nord = nBytesRead`, so a CHAR waveform's NORD includes it.
    #[test]
    fn a_waveform_reading_a_string_parameter_counts_its_nul() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = octet_param_adapter(b"AH501BE 1.0", ScanType::Passive);
        let mut rec = WaveformRecord::new(256, DbFieldType::Char);
        ads.init(&mut rec).unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 12);
        let EpicsValue::CharArray(v) = &rec.val else {
            panic!("CHAR waveform VAL")
        };
        assert_eq!(&v[..rec.nord as usize], b"AH501BE 1.0\0");
    }

    /// NELM is the waveform's valSize: at most `valSize - 1` bytes land
    /// (C `if (nBytesRead == pwf->nelm) nBytesRead--`).
    #[test]
    fn a_waveform_keeps_one_byte_less_than_nelm() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let mut ads = octet_param_adapter(b"AH501BE 1.0", ScanType::Passive);
        let mut rec = WaveformRecord::new(8, DbFieldType::Char);
        ads.init(&mut rec).unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.nord, 7);
        let EpicsValue::CharArray(v) = &rec.val else {
            panic!("CHAR waveform VAL")
        };
        assert_eq!(&v[..rec.nord as usize], b"AH501BE");
    }

    /// A stringin reads the text, its valSize is `sizeof(val)`, and an lsi
    /// takes `len = nBytesRead + 1` over bytes that already hold the NUL.
    #[test]
    fn stringin_and_lsi_take_a_string_parameter_as_c_does() {
        use epics_base_rs::server::records::lsi::LsiRecord;
        use epics_base_rs::server::records::stringin::StringinRecord;
        let mut ads = octet_param_adapter(b"AH501BE 1.0", ScanType::Passive);
        let mut si = StringinRecord::new("");
        ads.init(&mut si).unwrap();
        assert_eq!(ads.octet_max_size, MAX_STRING_SIZE);
        ads.read(&mut si).unwrap();
        assert_eq!(si.val(), Some(EpicsValue::String("AH501BE 1.0".into())));

        let mut ads = octet_param_adapter(b"AH501BE 1.0", ScanType::Passive);
        let mut lsi = LsiRecord::new("");
        ads.init(&mut lsi).unwrap();
        ads.read(&mut lsi).unwrap();
        assert_eq!(lsi.get_field("LEN"), Some(EpicsValue::ULong(13)));
        assert_eq!(lsi.val.text(), b"AH501BE 1.0\0");
    }

    /// `asyn:INITIAL_READBACK` on a stringout seeds VAL as C's `strcpy` does
    /// (devAsynOctet.c:373-376), and the next processing writes
    /// `my_strnlen(val, 40)` bytes (:958): the parameter's NUL never reaches the
    /// driver.
    #[test]
    fn stringout_initial_readback_then_proc_writes_no_nul() {
        use epics_base_rs::server::records::stringout::StringoutRecord;
        let mut ads = octet_param_adapter(b"abc", ScanType::Passive);
        ads.initial_readback = true;
        let mut so = StringoutRecord::new("");
        ads.init(&mut so).unwrap();
        assert_eq!(so.val(), Some(EpicsValue::String("abc".into())));

        let val = so.val().unwrap();
        match ads.write_op_pub(&val) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"abc"),
            other => panic!("{other:?}"),
        }
        ads.write(&mut so).unwrap();

        // A VAL holding a NUL some other way still stops there on the wire.
        let held = EpicsValue::String(epics_base_rs::types::PvString::from_bytes(b"ab\0c"));
        match ads.write_op_pub(&held) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"ab"),
            other => panic!("{other:?}"),
        }
    }

    /// An I/O Intr waveform takes what the fire counts: a parameter callback
    /// `strlen + 1` (asynPortDriver.cpp:817), an octet read fan-out its bytes
    /// alone (asynOctetBase).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_io_intr_waveform_takes_the_count_its_fire_carries() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;
        let gate = open_gate();
        let mut ads = octet_param_adapter(b"", ScanType::IoIntr);
        ads.bind_interrupt_gate(&gate);
        let mut rec = WaveformRecord::new(256, DbFieldType::Char);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let _rx = join_io_intr(&mut ads, &mut rec).expect("io intr receiver for IoIntr");

        for (iface, bytes, nord) in [
            (None, &b"AH501BE 1.0"[..], 12),
            (Some(InterfaceType::Octet), &b"abc\r\n"[..], 5),
        ] {
            interrupts.notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Octet(bytes.into()),
                timestamp: SystemTime::now(),
                iface,
                ..Default::default()
            });
            // Wait for the fire to land, then hand it back for the read.
            let entry = loop {
                if let Some(e) = ads.interrupt_fifo.lock().unwrap().pop().map(|d| d.entry) {
                    break e;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            ads.interrupt_fifo.lock().unwrap().push_with_overflow(entry);
            ads.read(&mut rec).unwrap();
            assert_eq!(rec.nord, nord, "{iface:?}");
        }
    }

    #[test]
    fn polled_read_transport_error_discards_value_keeps_prior() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_seeded_int32_adapter(5, crate::error::AsynStatus::Error);
        let mut rec = LonginRecord::new(0);
        rec.put_field("VAL", EpicsValue::Long(77)).unwrap(); // prior value
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(77)),
            "non-success device read must keep the prior VAL (C return -1), not store 5"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((1, 3)),
            "the alarm is still raised: READ_ALARM(1)/INVALID(3)"
        );
    }

    /// Boundary contrast to the discard case: a success device status on the same
    /// polled-read harness stores the value and raises no alarm.
    #[test]
    fn polled_read_success_stores_value() {
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_seeded_int32_adapter(5, crate::error::AsynStatus::Success);
        let mut rec = LonginRecord::new(0);
        rec.put_field("VAL", EpicsValue::Long(77)).unwrap();
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(5)),
            "a success device read stores the value"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            None,
            "success read raises no alarm"
        );
    }

    /// Init-time output readback (asyn:READBACK) seeds the record's value only on
    /// a successful read, mirroring C initAo/initLongout/initMbbo: `if (status ==
    /// asynSuccess) { pao->rval = value; return INIT_OK; }`, with
    /// `INIT_DO_NOT_CONVERT` the FAILURE fall-through (initAo
    /// devAsynInt32.c:955-959, initMbbo :1295-1299; initLo stores VAL under the
    /// same guard and returns INIT_OK either way, :1082-1086). A non-success
    /// initial read leaves the .db default — the init-time member of the
    /// aux_status value-store family.
    #[test]
    fn init_readback_transport_error_keeps_db_default() {
        use epics_base_rs::server::records::longout::LongoutRecord;
        let mut ads = make_seeded_int32_adapter(5, crate::error::AsynStatus::Error);
        ads.initial_readback = true;
        let mut rec = LongoutRecord::new(77); // .db default
        ads.init(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(77)),
            "a non-success initial read must leave the .db default (C INIT_DO_NOT_CONVERT)"
        );
    }

    /// Boundary contrast: a successful initial read seeds the device value.
    #[test]
    fn init_readback_success_seeds_value() {
        use epics_base_rs::server::records::longout::LongoutRecord;
        let mut ads = make_seeded_int32_adapter(5, crate::error::AsynStatus::Success);
        ads.initial_readback = true;
        let mut rec = LongoutRecord::new(77);
        ads.init(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(5)),
            "a successful initial read seeds the device value"
        );
    }

    /// An asynInt32 ao with a non-trivial ESLO/EOFF: the init seed must store
    /// the raw to RVAL and run the `raw -> eng` inverse, seeding VAL with the
    /// engineering value (C `initAo`: rval=value, INIT_OK → readback convert,
    /// devAsynInt32.c:947-957), NOT the raw counts.
    #[test]
    fn init_readback_ao_seeds_engineering_val_not_raw() {
        use epics_base_rs::server::records::ao::AoRecord;
        let mut ads = make_seeded_int32_adapter(5, crate::error::AsynStatus::Success);
        ads.initial_readback = true;
        let mut rec = AoRecord::new(0.0);
        rec.linr = 2; // LINEAR
        // convertAo over the default 0..65535 bounds: ESLO = 2, EOFF = 10.
        rec.egul = 10.0;
        rec.eguf = 2.0 * 65535.0 + 10.0;
        ads.init(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("RVAL"),
            Some(EpicsValue::Long(5)),
            "raw readback stored to RVAL"
        );
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!(
                (v - 20.0).abs() < 1e-9,
                "VAL = raw*ESLO + EOFF = 5*2 + 10 = 20 (engineering), got {v}"
            ),
            other => panic!("expected Double(20.0), got {other:?}"),
        }
    }

    /// The process-time driver readback (asyn:READBACK) for an asynInt32 ao:
    /// the ring-pop store routes the raw through `apply_raw_readback`, so VAL
    /// gets the engineering value and the outcome is `computed` (the framework
    /// then skips ao's forward convert via `set_device_did_compute`). Contrast
    /// the input ai path, which routes raw -> RVAL and returns `ok` (convert).
    #[test]
    fn io_intr_readback_ao_inverts_raw_to_engineering_val() {
        use epics_base_rs::server::records::ao::AoRecord;
        let mut ads = make_adapter(ScanType::Passive); // scalar asynInt32
        ads.set_asyn_readback(true); // output readback path
        let mut rec = AoRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        rec.linr = 2; // LINEAR
        rec.eslo = 2.0;
        rec.eoff = 10.0;
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(5),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "ao readback produces VAL itself → computed (framework skips the forward convert)"
        );
        assert_eq!(
            rec.get_field("RVAL"),
            Some(EpicsValue::Long(5)),
            "raw readback stored to RVAL"
        );
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!(
                (v - 20.0).abs() < 1e-9,
                "VAL = 5*2 + 10 = 20 (engineering), not the raw 5, got {v}"
            ),
            other => panic!("expected Double(20.0), got {other:?}"),
        }
    }

    /// An `asynInt32` mbbo (no ESLO) driver readback: the store routes the raw
    /// through `apply_raw_readback` (the dispatch is no longer ESLO-gated), so
    /// VAL is the resolved STATE INDEX, not the raw counts, and the outcome is
    /// `computed` (the framework skips the forward VAL->RVAL convert).
    #[test]
    fn io_intr_readback_mbbo_asynint32_maps_raw_to_state_index() {
        use epics_base_rs::server::records::mbbo::MbboRecord;
        let mut ads = make_adapter(ScanType::Passive); // scalar asynInt32
        ads.set_asyn_readback(true);
        let mut rec = MbboRecord::new(0);
        rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
        rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
        rec.init_record(0).unwrap(); // computes sdef=true
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap();
        rec.mask = 0x0C; // bits 2-3
        rec.shft = 2;
        // raw 0x08 → masked 0x08 → shifted 2 → state TWVL=2 → index 2.
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(0x08),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "mbbo readback resolves VAL itself → computed (framework skips forward convert)"
        );
        assert_eq!(rec.rval, 0x08, "RVAL keeps the masked (unshifted) raw");
        assert_eq!(rec.val, 2, "VAL is the state index, not the raw 8");
    }

    /// The same mbbo readback delivered on `asynUInt32Digital`: the dispatch
    /// covers both int-delivering ifaces with the one iface-agnostic hook, so a
    /// uint32digital mbbo readback also resolves through the state map (the
    /// gap-surveyor's untraced sibling — confirmed and covered).
    #[test]
    fn io_intr_readback_mbbo_uint32digital_routes_through_state_map() {
        use epics_base_rs::server::records::mbbo::MbboRecord;
        let mut ads = make_adapter(ScanType::Passive);
        ads.set_iface_type("asynUInt32Digital");
        ads.set_asyn_readback(true);
        let mut rec = MbboRecord::new(0);
        rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
        rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap();
        rec.mask = 0x0C;
        rec.shft = 2;
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::UInt32Digital(0x08),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "uint32digital mbbo readback computed"
        );
        assert_eq!(rec.rval, 0x08, "RVAL keeps the masked raw");
        assert_eq!(rec.val, 2, "VAL resolved through the state map, not raw 8");
    }

    /// An `asynInt32` bo (no ESLO, mask 0) driver readback: VAL = (raw != 0),
    /// RVAL = raw (unmasked, C `processBo` :1202-1203), `computed`.
    #[test]
    fn io_intr_readback_bo_asynint32_maps_raw_to_binary() {
        use epics_base_rs::server::records::bo::BoRecord;
        let mut ads = make_adapter(ScanType::Passive);
        ads.set_asyn_readback(true);
        let mut rec = BoRecord::new(0);
        ads.init(&mut rec).unwrap();
        // mask stays 0 (asynInt32 bo): RVAL = raw, VAL = (raw != 0).
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(0x2A),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute(), "bo readback computed");
        assert_eq!(rec.rval, 0x2A, "RVAL keeps the raw");
        assert_eq!(rec.val, 1, "VAL = (raw != 0) = 1, not the raw 42");
    }

    /// An `asynInt32` I/O Intr `bi` input: the device raw enters RVAL and
    /// biRecord's `rval -> 0/1` convert resolves VAL. C `processBi`
    /// (devAsynInt32.c) sets `rval = value` (mask 0) and returns 0. Before the
    /// fix the raw fell to the default `set_val` and landed in VAL verbatim
    /// (raw 5 → val 5); now VAL is the 0/1 the record exposes.
    #[test]
    fn io_intr_readback_bi_asynint32_maps_raw_to_binary() {
        use epics_base_rs::server::records::bi::BiRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // input, asynInt32
        let mut rec = BiRecord::new(0);
        ads.init(&mut rec).unwrap();
        // mask stays 0 (asynInt32 bi): RVAL = raw, VAL = (raw != 0).
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(5),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "bi readback resolves VAL itself → computed"
        );
        assert_eq!(rec.rval, 5, "RVAL keeps the unmasked raw");
        assert_eq!(rec.val, 1, "VAL = (raw != 0) = 1, not the raw 5");
    }

    /// An `asynUInt32Digital` I/O Intr `bi` input: `processBi` masks
    /// (`rval = value & mask`, devAsynUInt32Digital.c:689), then VAL =
    /// (rval != 0). The dispatch's one iface-agnostic hook covers this iface
    /// too. A high-bit raw resolves to 1 (not stored as 128).
    #[test]
    fn io_intr_readback_bi_uint32digital_applies_mask() {
        use epics_base_rs::server::records::bi::BiRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.set_iface_type("asynUInt32Digital");
        let mut rec = BiRecord::new(0);
        ads.init(&mut rec).unwrap();
        // Set MASK after init: `init` propagates the adapter's default full
        // mask (0xFFFFFFFF) onto the record (devAsynUInt32Digital MASK/SHFT
        // wiring). Override it to a single high bit to prove MASK gating.
        rec.mask = 0x80;
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::UInt32Digital(0x80),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute(), "uint32digital bi readback computed");
        assert_eq!(rec.rval, 0x80, "RVAL = raw & mask");
        assert_eq!(rec.val, 1, "high-bit mask hit → val 1, not 128");
        // A raw whose set bits fall entirely outside MASK → masked 0 → val 0.
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::UInt32Digital(0x01),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.rval, 0, "out-of-mask bits masked away");
        assert_eq!(rec.val, 0, "masked-to-zero raw → val 0");
    }

    /// An `asynUInt32Digital` I/O Intr `mbbiDirect` input (its only dset,
    /// `asynMbbiDirectUInt32Digital`): `processMbbiDirect` sets
    /// `rval = value & mask` (devAsynUInt32Digital.c:1032); mbbiDirectRecord
    /// resolves VAL = (masked >> SHFT) and the bit fields. Before the fix the
    /// raw landed in VAL verbatim (no MASK, no SHFT, wrong bits).
    #[test]
    fn io_intr_readback_mbbi_direct_uint32digital_maps_mask_shift_bits() {
        use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        ads.set_iface_type("asynUInt32Digital");
        let mut rec = MbbiDirectRecord::default();
        ads.init(&mut rec).unwrap();
        // Set MASK/SHFT after init: `init` propagates the adapter's default
        // full mask (0xFFFFFFFF → SHFT 0) onto the record. Override to bits 2-5
        // / SHFT 2 to exercise the mask + shift path.
        rec.mask = 0x3C; // bits 2-5
        rec.shft = 2;
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::UInt32Digital(0x3C),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute(), "mbbiDirect readback computed");
        assert_eq!(rec.rval, 0x3C, "RVAL keeps the masked (unshifted) raw");
        assert_eq!(
            rec.val, 0x0F,
            "VAL = (raw & mask) >> SHFT, not the raw 0x3C"
        );
        // The bits are the record's, derived once in C monitor().
        rec.set_device_did_compute(true);
        rec.process().unwrap();
        rec.monitor_value_changed();
        assert_eq!(rec.bits[0], 1);
        assert_eq!(rec.bits[3], 1);
    }

    /// The `apply_raw_readback` hook in isolation: given an already-positioned
    /// mask/shift, the device raw enters RVAL masked (C `processMbbi`
    /// `rval = value & mask`, devAsynInt32.c:1270) and mbbiRecord's convert
    /// shifts (>>SHFT) then resolves the state index — out-of-mask bits are
    /// stripped. (The init path that POSITIONS the mask is covered by
    /// `io_intr_readback_mbbi_asynint32_positions_nobt_mask`.)
    #[test]
    fn io_intr_readback_mbbi_asynint32_masks_and_maps_state() {
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // input, asynInt32
        let mut rec = MbbiRecord::new(0);
        rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
        rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
        rec.init_record(0).unwrap(); // computes sdef=true
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap();
        // Set an already-positioned mask/shift directly to exercise the hook
        // (init-path positioning is tested separately).
        rec.mask = 0x0C; // bits 2-3
        rec.shft = 2;
        // raw 0x88 -> masked 0x08 (0x80 stripped) -> shifted 2 -> TWVL=2 -> idx 2.
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(0x88),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute(), "mbbi readback computed");
        assert_eq!(
            rec.rval, 0x08,
            "RVAL = raw & mask (out-of-mask 0x80 stripped)"
        );
        assert_eq!(rec.val, 2, "state index 2 (TWVL), not leaked/unknown");
    }

    /// asynInt32 mbbi MASK positioning through the REAL init path. C
    /// devAsynInt32 initMbbi (devAsynInt32.c:1246-1247): `if(nobt==0)
    /// mask=0xffffffff; mask <<= shft`. mbbiRecord leaves MASK = (1<<NOBT)-1
    /// unshifted; the device support positions it. Goes through `init_record`
    /// then `ads.init` with NOBT/SHFT set (no manual `rec.mask =`), so it
    /// exercises the unshifted mask the real init path produces.
    #[test]
    fn io_intr_readback_mbbi_asynint32_positions_nobt_mask() {
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // asynInt32
        let mut rec = MbbiRecord::new(0);
        rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
        rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
        rec.put_field("THVL", EpicsValue::ULong(3)).unwrap();
        rec.put_field("NOBT", EpicsValue::UShort(4)).unwrap();
        rec.put_field("SHFT", EpicsValue::UShort(4)).unwrap();
        rec.init_record(0).unwrap(); // mbbiRecord: MASK = (1<<4)-1 = 0x0F unshifted
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap(); // positions MASK to 0x0F << 4 = 0xF0
        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::ULong(0xF0)),
            "asynInt32 mbbi MASK positioned: ((1<<NOBT)-1) << SHFT"
        );
        // value 0x30 = field 3 at bits 4-7 -> rval = 0x30 & 0xF0 = 0x30 -> >>4
        // = 3 -> THVL=3 -> state index 3. Before the fix the unshifted 0x0F
        // mask gave 0x30 & 0x0F = 0 -> index 0 (the SHFT>0 regression).
        ads.interrupt_fifo
            .lock()
            .unwrap()
            .push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(0x30),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute(), "mbbi readback computed");
        assert_eq!(rec.rval, 0x30, "RVAL = value & positioned mask");
        assert_eq!(rec.val, 3, "field 3 at bits 4-7 -> state index 3 (THVL)");
    }

    /// asynInt32 mbbi NOBT=0 edge: C initMbbi sets `mask=0xffffffff` before
    /// the shift (devAsynInt32.c:1246). mbbiRecord leaves MASK=0 for NOBT=0,
    /// so without the fallback `raw & 0` would zero every readback.
    #[test]
    fn mbbi_asynint32_nobt0_positions_full_mask() {
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = MbbiRecord::new(0);
        rec.put_field("NOBT", EpicsValue::UShort(0)).unwrap();
        rec.put_field("SHFT", EpicsValue::UShort(4)).unwrap();
        rec.init_record(0).unwrap(); // NOBT=0 -> mbbiRecord leaves MASK=0
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::ULong(0xFFFF_FFF0)),
            "NOBT=0 -> mask 0xffffffff << SHFT (C initMbbi), not 0"
        );
    }

    /// The same positioning applies to asynInt32 mbbo — R24's mbbo readback
    /// had the identical unshifted-mask gap (its `apply_raw_readback` also
    /// does `raw & mask`). One adapter owner positions both records, mirroring
    /// C devAsynInt32 initMbbo (devAsynInt32.c:1290-1291).
    #[test]
    fn mbbo_asynint32_positions_nobt_mask() {
        use epics_base_rs::server::records::mbbo::MbboRecord;
        let mut ads = make_adapter(ScanType::Passive); // output
        let mut rec = MbboRecord::new(0);
        rec.put_field("NOBT", EpicsValue::UShort(3)).unwrap();
        rec.put_field("SHFT", EpicsValue::UShort(5)).unwrap();
        rec.init_record(0).unwrap(); // mbboRecord: MASK = (1<<3)-1 = 0x07 unshifted
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap(); // positions to 0x07 << 5 = 0xE0
        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::ULong(0xE0)),
            "asynInt32 mbbo MASK positioned: ((1<<NOBT)-1) << SHFT"
        );
    }

    /// Positioning shifts the record's CURRENT mask, so a `.db`-set custom
    /// MASK (≠ (1<<NOBT)-1) is preserved and shifted — exactly C initMbbi,
    /// which does `pr->mask <<= shft` over the .db-loaded mask (mbbiRecord
    /// init only recomputes (1<<NOBT)-1 when MASK==0, mbbiRecord.c:129-130).
    /// Rebuilding from NOBT would clobber it (the Round 27 divergence).
    #[test]
    fn mbbi_asynint32_preserves_db_set_mask() {
        use epics_base_rs::server::records::mbbi::MbbiRecord;
        let mut ads = make_adapter(ScanType::IoIntr); // asynInt32
        let mut rec = MbbiRecord::new(0);
        // A .db that sets a custom (non-contiguous) MASK alongside NOBT/SHFT.
        rec.put_field("MASK", EpicsValue::ULong(0x05)).unwrap();
        rec.put_field("NOBT", EpicsValue::UShort(4)).unwrap();
        rec.put_field("SHFT", EpicsValue::UShort(4)).unwrap();
        rec.init_record(0).unwrap(); // MASK!=0 -> mbbiRecord leaves it 0x05
        rec.init_record(1).unwrap();
        ads.init(&mut rec).unwrap(); // positions the CURRENT mask: 0x05 << 4 = 0x50
        assert_eq!(
            rec.get_field("MASK"),
            Some(EpicsValue::ULong(0x50)),
            "custom .db MASK 0x05 shifted (0x05<<4), not rebuilt to ((1<<4)-1)<<4 = 0xF0"
        );
    }

    /// A `ScanType::IoIntr` adapter on `asynFloat64` whose driver exposes a
    /// `Float64` VAL param (so `init`/`drv_user_create` resolves `reason_set`).
    fn make_float64_io_intr_adapter() -> AsynDeviceSupport {
        struct F64Port {
            base: PortDriverBase,
        }
        impl F64Port {
            fn new() -> Self {
                let mut base = PortDriverBase::new(
                    "test_f64",
                    1,
                    PortFlags::default(),
                    InterfaceMask::UNMIGRATED_ALL,
                    InterfaceMask::UNMIGRATED_INTERRUPTS,
                );
                base.create_param("VAL", ParamType::Float64).unwrap();
                Self { base }
            }
        }
        impl PortDriver for F64Port {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let driver = F64Port::new();
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-f64-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            "test_f64".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        let link = AsynLink {
            port_name: "test_f64".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynFloat64");
        ads.set_record_info("TEST:AI", ScanType::IoIntr);
        ads
    }

    fn push_f64(ads: &AsynDeviceSupport, raw: f64) {
        let mut g = ads.interrupt_fifo.lock().unwrap();
        g.push_with_overflow(CachedInterrupt {
            value: crate::param::ParamValue::Float64(raw),
            timestamp: SystemTime::UNIX_EPOCH,
            alarm_status: 0,
            alarm_severity: 0,
            aux_status: crate::error::AsynStatus::Success,
            octet_nul: false,
        });
    }

    /// C `devAsynFloat64::processAi` (devAsynFloat64.c:594-604) computes the ai
    /// VAL itself: ASLO/AOFF scaling then the SMOO filter, then returns
    /// RTN_DO_NOT_CONVERT. The asyn-rs adapter returns `computed()` (skips the
    /// record convert) so it must apply the same ASLO/AOFF/SMOO — otherwise the
    /// raw driver double reaches VAL unscaled and unfiltered.
    #[test]
    fn float64_ai_read_applies_aslo_aoff_and_smoo() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_float64_io_intr_adapter();
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap(); // reason_set
        rec.put_field("ASLO", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("AOFF", EpicsValue::Double(1.0)).unwrap();
        rec.put_field("SMOO", EpicsValue::Double(0.0)).unwrap();

        // First read: SMOO=0 -> pure ASLO/AOFF: 10*2 + 1 = 21.
        push_f64(&ads, 10.0);
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(21.0)),
            "ASLO/AOFF applied: raw*aslo + aoff"
        );

        // Second read with SMOO=0.5: val = prev*smoo + val64*(1-smoo)
        //   val64 = 20*2 + 1 = 41; val = 21*0.5 + 41*0.5 = 31.
        rec.put_field("SMOO", EpicsValue::Double(0.5)).unwrap();
        push_f64(&ads, 20.0);
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(31.0)),
            "SMOO filter: prev*smoo + (raw*aslo+aoff)*(1-smoo)"
        );
    }

    /// SMOO primes on the first read — C skips smoothing while `pr->udf`
    /// (devAsynFloat64.c:599), so the first value is taken whole rather than
    /// smoothed toward the record's initial VAL.
    #[test]
    fn float64_ai_smoo_primes_on_first_read() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_float64_io_intr_adapter();
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        // SMOO=0.5 from the very first read; ASLO/AOFF identity.
        rec.put_field("SMOO", EpicsValue::Double(0.5)).unwrap();

        push_f64(&ads, 10.0);
        ads.read(&mut rec).unwrap();
        // Primed: VAL = 10, NOT 0*0.5 + 10*0.5 = 5.
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(10.0)),
            "first read primes the SMOO filter (no smoothing toward VAL=0)"
        );

        // Now smoothing engages: 10*0.5 + 20*0.5 = 15.
        push_f64(&ads, 20.0);
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Double(15.0)));
    }

    /// The process-time driver readback for an `asynFloat64` ao: the store path
    /// routes the raw double through `apply_float64_readback`, which applies the
    /// forward `ASLO`/`AOFF` scaling (`VAL = value*ASLO + AOFF`) and the outcome
    /// is `computed` (the framework skips ao's forward convert). C `processAo`
    /// (devAsynFloat64.c:647-649). ao carries no SMOO so it never enters the ai
    /// branch above.
    #[test]
    fn io_intr_readback_float64_ao_applies_aslo_aoff() {
        use epics_base_rs::server::records::ao::AoRecord;
        let mut ads = make_float64_io_intr_adapter();
        let mut rec = AoRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        rec.aslo = 2.0;
        rec.aoff = 1.0;

        push_f64(&ads, 10.0);
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute(),
            "float64 ao readback produces VAL itself → computed"
        );
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(21.0)),
            "VAL = raw*ASLO + AOFF = 10*2 + 1 = 21"
        );
    }

    /// An I/O-Intr sample the record's interface cannot read must alarm the
    /// record, never be coerced into it.
    ///
    /// The driver's parameter is `Float64` (what this `asynFloat64` ai binds),
    /// but a driver that fires a differently-typed value for the same reason —
    /// an on-demand driver that created the parameter as Int32, say — used to
    /// have that value mapped by its *variant*, not by the record's interface:
    /// `ParamValue::Int32` became `EpicsValue::Long`, which misses every
    /// `asynFloat64` arm of `store_read_value` and lands on raw `set_val`,
    /// silently bypassing the ai ASLO/AOFF/SMOO conversion the record is
    /// configured for.
    ///
    /// The polled read of that same mismatched parameter fails loudly
    /// (`AsynError::TypeMismatch` from `ParamList::get_float64` → READ/INVALID);
    /// the I/O-Intr read must do the same.
    #[test]
    fn io_intr_sample_of_the_wrong_type_alarms_instead_of_coercing() {
        use epics_base_rs::server::recgbl::alarm_status::READ_ALARM;
        use epics_base_rs::server::record::AlarmSeverity;
        use epics_base_rs::server::records::ai::AiRecord;

        let mut ads = make_float64_io_intr_adapter();
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        ads.set_record_info("TEST:AI", ScanType::IoIntr);
        rec.aslo = 2.0;
        rec.aoff = 1.0;

        // A correctly-typed sample: the ai conversion runs. VAL = 5*2 + 1 = 11.
        push_f64(&ads, 5.0);
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(11.0)),
            "asynFloat64 ai applies ASLO/AOFF: 5*2 + 1 = 11"
        );
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            None,
            "a well-typed sample raises no alarm"
        );

        // Same reason, a value this record's interface cannot read.
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.push_with_overflow(CachedInterrupt {
                value: crate::param::ParamValue::Int32(5),
                timestamp: SystemTime::UNIX_EPOCH,
                alarm_status: 0,
                alarm_severity: 0,
                aux_status: crate::error::AsynStatus::Success,
                octet_nul: false,
            });
        }
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((READ_ALARM, AlarmSeverity::Invalid as u16)),
            "an Int32 sample on an asynFloat64 record is a TypeMismatch: READ/INVALID, \
             the same alarm the polled read raises"
        );
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(11.0)),
            "the prior value is kept; the raw 5 must NOT be coerced into VAL \
             (which would bypass ASLO/AOFF and read 5 instead of 11)"
        );
    }

    /// The device write for an `asynFloat64` ao reverses `ASLO`/`AOFF` and
    /// anchors on the OROC-rate-limited `OVAL`: `device = (OVAL - AOFF) / ASLO`
    /// (C `processAo`, devAsynFloat64.c:651-654) — the inverse of the readback.
    /// Together they round-trip: a value written out, read back, re-scaled
    /// returns the same value.
    #[test]
    fn float64_ao_write_reverses_aslo_aoff_and_round_trips() {
        use epics_base_rs::server::records::ao::AoRecord;
        let ads = make_float64_io_intr_adapter();
        let mut rec = AoRecord::new(0.0);
        rec.aslo = 2.0;
        rec.aoff = 1.0;
        rec.oval = 21.0; // post-OROC output the device write anchors on
        // OVAL = 21 -> device = (21 - 1) / 2 = 10.
        let written = ads.device_output_value(&rec).unwrap();
        assert_eq!(
            written,
            EpicsValue::Double(10.0),
            "device = (OVAL - AOFF) / ASLO = (21 - 1) / 2 = 10"
        );
        // Round-trip: that device value, read back, re-scales to the original.
        let mut readback = AoRecord::new(0.0);
        readback.aslo = 2.0;
        readback.aoff = 1.0;
        let EpicsValue::Double(dev) = written else {
            panic!("expected Double");
        };
        assert!(readback.apply_float64_readback(dev));
        assert_eq!(
            readback.val(),
            Some(EpicsValue::Double(21.0)),
            "readback of (OVAL-AOFF)/ASLO re-scales to the original output"
        );
    }

    /// The asynFloat64 ao write anchors on OVAL (post-OROC), not VAL: when OROC
    /// has rate-limited the output, the device receives the ramped OVAL while
    /// VAL holds the (jumped) setpoint. C `processAo` uses `pr->oval`
    /// (devAsynFloat64.c:651).
    #[test]
    fn float64_ao_write_anchors_on_oval_not_val() {
        use epics_base_rs::server::records::ao::AoRecord;
        let ads = make_float64_io_intr_adapter();
        let mut rec = AoRecord::new(0.0);
        // ASLO=1/AOFF=0 (identity scaling) to isolate the OVAL-vs-VAL anchor.
        rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap(); // setpoint
        rec.oval = 40.0; // OROC ramped only partway this cycle
        assert_eq!(
            ads.device_output_value(&rec).unwrap(),
            EpicsValue::Double(40.0),
            "device receives the OROC-rate-limited OVAL (40), not the VAL setpoint (100)"
        );
    }

    /// The default ao (ASLO=1, AOFF=0) is an identity in both directions — no
    /// behaviour change for the unconfigured float64 ao.
    #[test]
    fn float64_ao_default_scaling_is_identity() {
        use epics_base_rs::server::records::ao::AoRecord;
        let mut ads = make_float64_io_intr_adapter();
        let mut rec = AoRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        // Defaults: ASLO=1.0, AOFF=0.0.
        push_f64(&ads, 7.5);
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(7.5)),
            "default readback is identity (raw*1 + 0)"
        );
        rec.oval = 7.5;
        assert_eq!(
            ads.device_output_value(&rec).unwrap(),
            EpicsValue::Double(7.5),
            "default write is identity ((7.5-0)/1)"
        );
    }

    /// An asynInt32 ao writes its raw RVAL (the convert output from OVAL), not
    /// the engineering VAL. C `processAo` writes `pr->rval`
    /// (devAsynInt32.c:997). With a LINEAR conversion VAL≠RVAL, so this is
    /// observable. (RVAL = convert(VAL) is exercised by the ao convert/readback
    /// tests; here it is seeded directly to isolate the device-write anchor.)
    #[test]
    fn int32_ao_write_sends_rval_not_eng_val() {
        use epics_base_rs::server::records::ao::AoRecord;
        let ads = make_adapter(ScanType::Passive); // asynInt32
        let mut rec = AoRecord::new(0.0);
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap(); // engineering
        rec.rval = 20; // raw counts (e.g. ESLO=0.5: 10 / 0.5 = 20)
        assert_eq!(
            ads.device_output_value(&rec).unwrap(),
            EpicsValue::Long(20),
            "device receives RVAL (20 counts), not eng VAL (10)"
        );
    }

    /// An asynInt32 mbbo writes its state-mapped RVAL, not the VAL index. C
    /// `processMbbo` writes `pr->rval` (devAsynInt32.c:1334). RVAL is ULong and
    /// must be coerced to the Int32 write type.
    #[test]
    fn int32_mbbo_write_sends_state_rval_not_index() {
        use epics_base_rs::server::records::mbbo::MbboRecord;
        let ads = make_adapter(ScanType::Passive); // asynInt32
        let mut rec = MbboRecord::new(0);
        // RVAL holds the state value (e.g. 0x2A) while VAL holds the index.
        rec.put_field("RVAL", EpicsValue::ULong(0x2A)).unwrap();
        assert_eq!(
            ads.device_output_value(&rec).unwrap(),
            EpicsValue::Long(0x2A),
            "device receives the state-mapped RVAL (0x2A), not the VAL index"
        );
    }

    /// longout carries no RVAL (VAL *is* the raw), so the device write stays on
    /// VAL — matching C `processLongout` (writes `pr->val`). Confirms the
    /// RVAL-anchor does not over-reach to conversion-less outputs.
    #[test]
    fn int32_longout_write_keeps_val() {
        use epics_base_rs::server::records::longout::LongoutRecord;
        let ads = make_adapter(ScanType::Passive); // asynInt32
        let rec = LongoutRecord::new(77);
        assert!(rec.get_field("RVAL").is_none(), "longout has no RVAL");
        assert_eq!(
            ads.device_output_value(&rec).unwrap(),
            EpicsValue::Long(77),
            "longout device write stays on VAL (no RVAL to anchor)"
        );
    }

    /// PR #162: `aai` (array analog input) and `aao` (array analog output)
    /// records use direction-specific DTYPs like `asynFloat64ArrayIn` and
    /// `asynFloat64ArrayOut` that must collapse to the underlying interface
    /// `asynFloat64Array` for the adapter dispatch. Without this the read_op
    /// and write_op matchers (around L383/L417/L489) miss the DTYP and fail
    /// to bind the record.
    #[test]
    fn dtyp_normalize_aai_aao_array_in_out() {
        // Float64 — most common aai/aao pattern.
        assert_eq!(
            normalize_asyn_dtyp("asynFloat64ArrayIn"),
            "asynFloat64Array"
        );
        assert_eq!(
            normalize_asyn_dtyp("asynFloat64ArrayOut"),
            "asynFloat64Array"
        );
        // Int32 family — covers waveform/aai/aao integer variants.
        assert_eq!(normalize_asyn_dtyp("asynInt32ArrayIn"), "asynInt32Array");
        assert_eq!(normalize_asyn_dtyp("asynInt32ArrayOut"), "asynInt32Array");
        // Other widths sanity-check the suffix rule, not exhaustive of C asyn.
        assert_eq!(normalize_asyn_dtyp("asynInt8ArrayIn"), "asynInt8Array");
        assert_eq!(normalize_asyn_dtyp("asynInt16ArrayOut"), "asynInt16Array");
        assert_eq!(normalize_asyn_dtyp("asynInt64ArrayIn"), "asynInt64Array");
        assert_eq!(
            normalize_asyn_dtyp("asynFloat32ArrayOut"),
            "asynFloat32Array"
        );
    }

    #[test]
    fn dtyp_normalize_preserves_non_array_dtyps() {
        // Scalar DTYPs must pass through unchanged — only Array/Octet
        // direction suffixes are stripped.
        assert_eq!(normalize_asyn_dtyp("asynInt32"), "asynInt32");
        assert_eq!(normalize_asyn_dtyp("asynFloat64"), "asynFloat64");
        // Octet read/write direction-specific DTYPs collapse to asynOctet
        // (C EPICS lsi/lso/printf adapter convention).
        assert_eq!(normalize_asyn_dtyp("asynOctetRead"), "asynOctet");
        assert_eq!(normalize_asyn_dtyp("asynOctetWrite"), "asynOctet");
        // asynOctetCmdResponse also collapses to asynOctet — the write-then-read
        // is carried by octet_cmd, not a distinct interface.
        assert_eq!(normalize_asyn_dtyp("asynOctetCmdResponse"), "asynOctet");
        assert_eq!(normalize_asyn_dtyp("asynOctetWriteRead"), "asynOctet");
        // asynOctetWriteBinary likewise — write-only/no-NUL-trim is carried by
        // octet_binary, not a distinct interface.
        assert_eq!(normalize_asyn_dtyp("asynOctetWriteBinary"), "asynOctet");
        // TimeSeries DTYPs map to their base interface — the scalar-into-array
        // accumulation is carried by `time_series`, not a distinct interface.
        assert_eq!(normalize_asyn_dtyp("asynInt32TimeSeries"), "asynInt32");
        assert_eq!(normalize_asyn_dtyp("asynFloat64TimeSeries"), "asynFloat64");
        assert_eq!(normalize_asyn_dtyp("asynInt64TimeSeries"), "asynInt64");
    }

    /// Mock octet port for asynOctetWriteBinary / asynOctetWrite: records every
    /// write payload and accepts any DRVINFO as a param (useDrvUser=1, so init's
    /// drv_user_create succeeds and the write-only read() path runs).
    struct BinaryWritePort {
        base: PortDriverBase,
        writes: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        /// When true, every write fails with a Timeout status (to exercise the
        /// write-only alarm path).
        fail: bool,
    }
    impl PortDriver for BinaryWritePort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        // useDrvUser=1: the DRVINFO is a real param. Accept any name (reason 0) so
        // the test need not pre-register params on the mock.
        fn drv_user_create(
            &mut self,
            _req: &DrvUserRequest,
        ) -> AsynResult<crate::port::DrvUserInfo> {
            Ok(crate::port::DrvUserInfo::from_reason(0))
        }
        // A defined driver value, so an initial readback that DOES run
        // succeeds and is visible in the record (the gate tests below).
        fn io_read_octet(
            &mut self,
            _user: &AsynUser,
            buf: &mut [u8],
        ) -> crate::interpose::OctetReadResult {
            let v = b"DRIVER";
            let n = v.len().min(buf.len());
            buf[..n].copy_from_slice(&v[..n]);
            crate::interpose::OctetReadResult::ok(n, crate::interpose::EomReason::empty())
        }
        fn io_read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
            Ok(7)
        }
        fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            if self.fail {
                // A generic asynError (not a specific Timeout/Overflow/…): this
                // is the only arm whose alarm depends on the direction default,
                // so it exercises the WRITE_ALARM-for-output mapping.
                return Err(AsynError::Status {
                    status: crate::protocol::status::AsynStatus::Error,
                    message: "mock write error".into(),
                });
            }
            self.writes.lock().unwrap().push(data.to_vec());
            Ok(data.len())
        }
    }

    /// Spawn a [`BinaryWritePort`] actor; returns its handle + the write recorder.
    fn spawn_binary_write_port(name: &str) -> (PortHandle, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
        spawn_binary_write_port_inner(name, false)
    }

    /// Spawn a [`BinaryWritePort`] whose writes always fail (Timeout).
    fn spawn_failing_write_port(name: &str) -> PortHandle {
        spawn_binary_write_port_inner(name, true).0
    }

    fn spawn_binary_write_port_inner(
        name: &str,
        fail: bool,
    ) -> (PortHandle, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = BinaryWritePort {
            base: PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            writes: writes.clone(),
            fail,
        };
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(port), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("binwrite-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            name.into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        (handle, writes)
    }

    /// asynOctetWriteBinary writes the full NORD bytes — INCLUDING an interior NUL
    /// — through the whole factory→init→read path, matching C callbackWfWriteBinary
    /// (devAsynOctet.c:1086-1091, writeIt(bptr, nord), no my_strnlen trim).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn octet_write_binary_writes_full_nord_bytes_including_interior_nul() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (handle, writes) = spawn_binary_write_port("binwrite_wb");
        crate::asyn_record::register_port("binwrite_wb", handle, &crate::registry::open_owner())
            .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynOctetWriteBinary",
            inp: "@asyn(binwrite_wb,0)REG",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        // NORD = 3, with an interior NUL; asynOctetWrite would trim at the NUL.
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02].into()))
            .unwrap();
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![vec![0x01, 0x00, 0x02]],
            "binary write must send the full NORD bytes, interior NUL included"
        );
    }

    /// An octet OUTPUT record keeps its .db VAL through init: C devAsynOctet
    /// reads an output's initial value only under info(asyn:INITIAL_READBACK,
    /// "1") (devAsynOctet.c:357-380); `initSoWrite` queues no ASYN_INIT read.
    /// Before the gate, the auto-readback pulled the driver's value ("DRIVER"
    /// here, "" for a Gather port's seeded NDARRAY_PORT) over the .db VAL, so
    /// NDPluginBase.template's NDArrayPort macro never reached PINI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stringout_keeps_its_db_val_without_the_readback_tag() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::stringout::StringoutRecord;

        let (handle, _writes) = spawn_binary_write_port("octet_no_initrb");
        crate::asyn_record::register_port(
            "octet_no_initrb",
            handle,
            &crate::registry::open_owner(),
        )
        .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "stringout",
            dtyp: "asynOctetWrite",
            inp: "",
            out: "@asyn(octet_no_initrb,0)NDARRAY_PORT",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");
        let mut rec = StringoutRecord::new("FROMDB");
        dev.init(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("VAL").unwrap(),
            EpicsValue::String("FROMDB".into()),
            "an octet output's .db VAL must survive init without asyn:INITIAL_READBACK"
        );
    }

    /// Contrast: the Int32 families DO read back at init (C initAo/initBo/
    /// initLongout/initMbbo queue an ASYN_INIT read), so the octet gate must
    /// not widen to them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_longout_still_reads_back_its_initial_value() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::longout::LongoutRecord;

        let (handle, _writes) = spawn_binary_write_port("int32_initrb");
        crate::asyn_record::register_port("int32_initrb", handle, &crate::registry::open_owner())
            .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "longout",
            dtyp: "asynInt32",
            inp: "",
            out: "@asyn(int32_initrb,0)REG",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");
        let mut rec = LongoutRecord::new(0);
        dev.init(&mut rec).unwrap();
        assert_eq!(
            rec.get_field("VAL").unwrap(),
            EpicsValue::Long(7),
            "an asynInt32 output must still seed VAL from the driver at init"
        );
    }

    /// Contrast: the text asynOctetWrite trims the same payload at the first NUL
    /// (C callbackWfWrite my_strnlen) — proving octet_binary is the only switch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn octet_write_text_trims_at_first_nul() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (handle, writes) = spawn_binary_write_port("binwrite_text");
        crate::asyn_record::register_port("binwrite_text", handle, &crate::registry::open_owner())
            .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynOctetWrite",
            inp: "@asyn(binwrite_text,0)REG",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02].into()))
            .unwrap();
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![vec![0x01]],
            "text write must trim at the first NUL"
        );
    }

    /// A (record type, DTYP) pair no asyn `device()` line declares gets no
    /// device from the universal factory, so base reports the record as
    /// having no device support — C's `Can't set 'X.DTYP'` at db load. A
    /// `calcout` with `DTYP asynInt32` and an `@asyn` OUT used to bind an
    /// asynInt32 output dset the record type never calls, so it loaded
    /// cleanly, computed OVAL and wrote nothing to the driver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn factory_refuses_a_record_type_its_dbd_never_declares() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;

        let (handle, _writes) = spawn_binary_write_port("undeclared_rt");
        crate::asyn_record::register_port("undeclared_rt", handle, &crate::registry::open_owner())
            .unwrap();

        let bind = |record_type: &str, dtyp: &str| {
            universal_asyn_factory(&DeviceSupportContext {
                record_type,
                dtyp,
                inp: "",
                out: "@asyn(undeclared_rt,0)NUM_POINTS",
            })
            .is_some()
        };
        assert!(
            !bind("calcout", "asynInt32"),
            "no device(calcout, ...) line"
        );
        assert!(
            bind("longout", "asynInt32"),
            "the control: device(longout, INST_IO, asynLoInt32, \"asynInt32\")"
        );
        // The rule is the pair, not the record type alone: ao declares no
        // asynOctetWrite, busy (from the busy module) declares asynInt32.
        assert!(
            !bind("ao", "asynOctetWrite"),
            "no device(ao, ..., \"asynOctetWrite\") line"
        );
        assert!(
            bind("busy", "asynInt32"),
            "busySupport_withASYN.dbd declares it"
        );
    }

    /// The factory's gate and the menus base lists read one table: every
    /// choice `register_asyn_device_menus` publishes for a record type is a
    /// pair the factory binds, and the record types are the generated list
    /// plus busy.
    #[test]
    fn asyn_device_menu_is_the_generated_table_plus_busy() {
        for &rt in crate::dbd_generated::DEVICE_MENU_RECORD_TYPES {
            assert!(std::ptr::eq(
                asyn_device_menu(rt).unwrap(),
                crate::dbd_generated::devices(rt).unwrap()
            ));
            assert!(ASYN_DEVICE_MENU_RECORD_TYPES.contains(&rt));
        }
        let busy = asyn_device_menu("busy").unwrap();
        assert_eq!(
            busy.iter()
                .map(|row| (row.name, row.choice, row.link_type))
                .collect::<Vec<_>>(),
            [(
                "asynBusyInt32",
                "asynInt32",
                epics_base_rs::server::record::DbLinkType::InstIo
            )]
        );
        assert!(ASYN_DEVICE_MENU_RECORD_TYPES.contains(&"busy"));
        assert_eq!(
            ASYN_DEVICE_MENU_RECORD_TYPES.len(),
            crate::dbd_generated::DEVICE_MENU_RECORD_TYPES.len() + 1
        );
        assert!(asyn_device_menu("calcout").is_none());
    }

    /// Mock array port: records every float32 array write and accepts any
    /// DRVINFO as a param. Reads return zeros, so a record mis-classified as an
    /// input would read 2048 zeros back instead of writing.
    struct Float32ArrayPort {
        base: PortDriverBase,
        writes: Arc<std::sync::Mutex<Vec<Vec<f32>>>>,
    }
    impl PortDriver for Float32ArrayPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn drv_user_create(
            &mut self,
            _req: &DrvUserRequest,
        ) -> AsynResult<crate::port::DrvUserInfo> {
            Ok(crate::port::DrvUserInfo::from_reason(0))
        }
        fn read_float32_array(&mut self, _user: &AsynUser, buf: &mut [f32]) -> AsynResult<usize> {
            buf.fill(0.0);
            Ok(buf.len())
        }
        fn write_float32_array(&mut self, _user: &AsynUser, data: &[f32]) -> AsynResult<()> {
            self.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    fn spawn_float32_array_port(name: &str) -> (PortHandle, Arc<std::sync::Mutex<Vec<Vec<f32>>>>) {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = Float32ArrayPort {
            base: PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            writes: writes.clone(),
        };
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(port), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("f32array-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            name.into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        (handle, writes)
    }

    /// `waveform` + `DTYP asynFloat32ArrayOut` + INP (the C measComp
    /// `WaveGen<n>UserWF` form) is an output: processing writes the NORD
    /// elements through `write_float32_array` and leaves VAL as put. The
    /// factory used to admit only the two asynOctet write DTYPs as INP-bound
    /// outputs, so this record read the driver instead and VAL came back as
    /// NELM zeros. C: `MAKE_DEVSUP(asynFloat32ArrayWfOut, waveformRecord, inp,
    /// ..., true)`, devAsynXXXArray.cpp:570 via the macro at :475-505.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn array_out_dtyp_on_inp_link_writes_the_driver() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (handle, writes) = spawn_float32_array_port("f32arr_out");
        crate::asyn_record::register_port("f32arr_out", handle, &crate::registry::open_owner())
            .unwrap();

        let ctx = DeviceSupportContext {
            record_type: "waveform",
            dtyp: "asynFloat32ArrayOut",
            inp: "@asyn(f32arr_out,0)USER_WF",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = WaveformRecord::new(2048, DbFieldType::Float);
        rec.put_field("VAL", EpicsValue::FloatArray(vec![0.5, 0.25, 0.125].into()))
            .unwrap();
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![vec![0.5, 0.25, 0.125]],
            "the driver must receive the NORD elements once"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::FloatArray(vec![0.5, 0.25, 0.125].into())),
            "VAL must keep the put values, not the driver readback"
        );
    }

    /// The direction a DTYP fixes by name: every `asynXxxArrayOut` and the two
    /// asynOctet write dsets are INP-bound outputs; direction-agnostic scalar
    /// DTYPs and the `In` array forms are not.
    #[test]
    fn dtyp_is_output_names_every_inp_bound_output_dset() {
        for d in [
            "asynInt8ArrayOut",
            "asynInt16ArrayOut",
            "asynInt32ArrayOut",
            "asynInt64ArrayOut",
            "asynFloat32ArrayOut",
            "asynFloat64ArrayOut",
            "asynOctetWrite",
            "asynOctetWriteBinary",
        ] {
            assert!(dtyp_is_output(d), "{d} is an output");
        }
        for d in [
            "asynFloat32ArrayIn",
            "asynInt32ArrayIn",
            "asynOctetRead",
            "asynOctetCmdResponse",
            "asynInt32",
            "asynFloat64",
            "asynInt32TimeSeries",
        ] {
            assert!(!dtyp_is_output(d), "{d} takes its direction from the link");
        }
    }

    /// binary_write_op emits a plain OctetWrite (EOS-appending), NOT
    /// OctetWriteBinary (EOS-suppressing). devAsynOctet does no EOS manipulation;
    /// routing to OctetWriteBinary would wrongly strip the port's configured OEOS.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn octet_write_binary_op_is_plain_octetwrite_not_eos_suppressed() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (handle, _writes) = spawn_binary_write_port("binwrite_op");
        let link = AsynLink {
            port_name: "binwrite_op".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "REG".into(),
        };
        let ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02].into()))
            .unwrap();
        let val = rec.val().unwrap();
        let op = ads.binary_write_op(&rec, &val);
        match op {
            Some(RequestOp::OctetWrite { data }) => {
                assert_eq!(data, vec![0x01, 0x00, 0x02], "full NORD bytes, no trim");
            }
            other => panic!("expected plain OctetWrite (EOS appended), got {other:?}"),
        }
    }

    /// A failing octet write on the write-only path must surface the driver's
    /// alarm via last_alarm(), matching C callbackWfWrite/WriteBinary
    /// (writeIt -> result.status -> finish recGblSetSevr). The path previously
    /// swallowed the submit result, so a write failure raised no alarm. Shared by
    /// asynOctetWrite (tested here) and asynOctetWriteBinary.
    ///
    /// The mock fails with a generic asynError, whose alarm is the only one that
    /// depends on the direction default: because this is an output (isOutput=1)
    /// record, C maps it to WRITE_ALARM, not READ_ALARM (processCommon,
    /// devAsynOctet.c:806-807).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn octet_write_failure_raises_write_alarm_on_write_only_path() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let handle = spawn_failing_write_port("binwrite_fail");
        let link = AsynLink {
            port_name: "binwrite_fail".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "REG".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.write_only = true;
        ads.reason_set = true;
        ads.set_record_info("TEST:WFAIL", ScanType::Passive);

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x41, 0x42].into()))
            .unwrap();
        ads.read(&mut rec).unwrap();

        // Generic asynError on an output record -> WRITE_ALARM/INVALID (NOT
        // READ_ALARM, which the path used before the direction fix).
        assert_eq!(
            ads.last_alarm().map(|a| (a.stat, a.sevr)),
            Some((
                epics_base_rs::server::recgbl::alarm_status::WRITE_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid as u16
            )),
            "a write failure on an output record must raise WRITE_ALARM, not be swallowed or READ_ALARM"
        );
    }

    /// Mock octet port for asynOctetCmdResponse: records every write payload and
    /// the flush/write/read call order, and returns a fixed reply on read.
    struct CmdResponsePort {
        base: PortDriverBase,
        writes: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        sequence: Arc<std::sync::Mutex<Vec<&'static str>>>,
        reply: Vec<u8>,
    }
    impl PortDriver for CmdResponsePort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
            self.sequence.lock().unwrap().push("flush");
            Ok(())
        }
        fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            self.writes.lock().unwrap().push(data.to_vec());
            self.sequence.lock().unwrap().push("write");
            Ok(data.len())
        }
        fn io_read_octet(
            &mut self,
            _user: &AsynUser,
            buf: &mut [u8],
        ) -> crate::interpose::OctetReadResult {
            self.sequence.lock().unwrap().push("read");
            let n = self.reply.len().min(buf.len());
            buf[..n].copy_from_slice(&self.reply[..n]);
            crate::interpose::OctetReadResult::ok(n, crate::interpose::EomReason::empty())
        }
    }

    type CmdRecorder = (
        Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        Arc<std::sync::Mutex<Vec<&'static str>>>,
    );

    /// Spawn a [`CmdResponsePort`] actor and return its [`PortHandle`] plus the
    /// shared write-payload / call-order recorders.
    fn spawn_cmd_response_port(name: &str, reply: &[u8]) -> (PortHandle, CmdRecorder) {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sequence = Arc::new(std::sync::Mutex::new(Vec::new()));
        let port = CmdResponsePort {
            base: PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            writes: writes.clone(),
            sequence: sequence.clone(),
            reply: reply.to_vec(),
        };
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(port), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("cmdresp-actor".into())
            .spawn(actor.registered())
            .unwrap();
        let handle = PortHandle::new(
            tx,
            name.into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );
        (handle, (writes, sequence))
    }

    /// `read_op` emits a write-then-read (`OctetWriteRead`) carrying the cached
    /// command when one is present (asynOctetCmdResponse), and a plain
    /// `OctetRead` otherwise (asynOctetRead) — the one routing decision the
    /// feature adds.
    #[test]
    fn read_op_selects_write_read_only_when_command_present() {
        let (handle, _) = spawn_cmd_response_port("cmdresp_readop", b"");
        let link = AsynLink {
            port_name: "cmdresp_readop".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_max_size = 128;

        // No command → plain read.
        match ads.read_op(&epics_base_rs::server::records::ai::AiRecord::new(0.0)) {
            Some(RequestOp::OctetRead { buf_size, .. }) => assert_eq!(buf_size, 128),
            other => panic!("expected OctetRead, got {other:?}"),
        }

        // Command cached → write-then-read carrying the command bytes.
        ads.octet_cmd = Some(b"*IDN?\r\n".to_vec());
        match ads.read_op(&epics_base_rs::server::records::ai::AiRecord::new(0.0)) {
            Some(RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
                ..
            }) => {
                assert_eq!(data, b"*IDN?\r\n".to_vec());
                assert_eq!(buf_size, 128);
                // C devAsynOctet command-response does NOT flush before the write
                // (raw writeIt → readIt), unlike asynOctetSyncIO::writeRead.
                assert!(!flush, "CmdResponse must not pre-flush the input buffer");
            }
            other => panic!("expected OctetWriteRead, got {other:?}"),
        }
    }

    /// The per-record octet length cap (`octet_len_cap`, the asyn-rs home for C
    /// `modbusDrvUser_t.len`) truncates the octet WRITE payload,
    /// mirroring C `writeOctet`'s `getStringLen(maxChars) = min(data_len, len)`
    /// (drvModbusAsyn.cpp:1521/1524/1531). SIZV / `octet_max_size` does NOT bound
    /// writes; `None` (C `len == -1`) leaves the payload intact. Covers the text,
    /// CharArray, and over-long-cap edges.
    #[test]
    fn octet_len_cap_truncates_write_payload() {
        let (handle, _) = spawn_cmd_response_port("octetcap_write", b"");
        let link = AsynLink {
            port_name: "octetcap_write".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        // A large read buffer must NOT bound the write (C maxChars is data length).
        ads.octet_max_size = 256;

        // No cap (C len == -1): full payload written.
        ads.octet_len_cap = None;
        match ads.write_op(&EpicsValue::String("ABCDEFG".into())) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"ABCDEFG".to_vec()),
            other => panic!("expected OctetWrite, got {other:?}"),
        }

        // LEN=3: write truncated to 3 bytes (C getStringLen min(7,3)=3).
        ads.octet_len_cap = Some(3);
        match ads.write_op(&EpicsValue::String("ABCDEFG".into())) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"ABC".to_vec()),
            other => panic!("expected OctetWrite, got {other:?}"),
        }

        // CharArray write (waveform FTVL=CHAR) is capped on the same path.
        match ads.write_op(&EpicsValue::CharArray(b"ABCDEFG".to_vec().into())) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"ABC".to_vec()),
            other => panic!("expected OctetWrite, got {other:?}"),
        }

        // A cap longer than the data is a no-op (min(7,99)=7).
        ads.octet_len_cap = Some(99);
        match ads.write_op(&EpicsValue::String("ABCDEFG".into())) {
            Some(RequestOp::OctetWrite { data }) => assert_eq!(data, b"ABCDEFG".to_vec()),
            other => panic!("expected OctetWrite, got {other:?}"),
        }
    }

    /// A poller-fired octet value (delivered as `EpicsValue::String`) is capped at
    /// `octet_max_size` (= min(SIZV, LEN)),
    /// mirroring C's poller `getStringLen(sizeof stringBuffer) = min(buffer, len)`
    /// (drvModbusAsyn.cpp:1914). The polled read caps at the driver via `buf_size`;
    /// the I/O-Intr ring fires the full block, so the cap is applied consumer-side.
    /// Array-interface `CharArray` reads are left untouched.
    #[test]
    fn octet_max_size_caps_io_intr_read_value() {
        let (handle, _) = spawn_cmd_response_port("octetcap_read", b"");
        let link = AsynLink {
            port_name: "octetcap_read".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_max_size = 5;

        // Over the cap → truncated to 5 bytes.
        match ads.cap_octet_read_value(EpicsValue::String("ABCDEFG".into())) {
            EpicsValue::String(s) => assert_eq!(s.as_bytes(), b"ABCDE"),
            other => panic!("expected String, got {other:?}"),
        }
        // Under the cap → untouched.
        match ads.cap_octet_read_value(EpicsValue::String("AB".into())) {
            EpicsValue::String(s) => assert_eq!(s.as_bytes(), b"AB"),
            other => panic!("expected String, got {other:?}"),
        }
        // Array-interface CharArray reads are NOT octet-capped (bounded by
        // max_array_elements elsewhere).
        let long = vec![1u8; 10];
        match ads.cap_octet_read_value(EpicsValue::CharArray(long.clone().into())) {
            EpicsValue::CharArray(v) => assert_eq!(v, long),
            other => panic!("expected CharArray, got {other:?}"),
        }
    }

    /// R7-46: C `devAsynOctet::readIt` (devAsynOctet.c:693-717) passes the
    /// record's own value buffer to `pasynOctet->read`, so a partial line
    /// followed by a timeout leaves those bytes *in the record* (`psi->val`)
    /// and returns the failing status; `processCommon` maps it to
    /// TIMEOUT_ALARM/INVALID (:806-808). The port's error branch mapped the
    /// alarm but dropped the bytes, because the transfer never left the
    /// driver's buffer.
    #[test]
    fn octet_partial_read_stores_the_bytes_and_the_timeout_alarm() {
        use crate::error::{AsynError, AsynResult, AsynStatus};
        use crate::interpose::{EomReason, OctetReadResult};
        use crate::user::AsynUser;
        use epics_base_rs::server::records::stringin::StringinRecord;

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
                OctetReadResult {
                    status: Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
                    }),
                    ..OctetReadResult::ok(3, EomReason::empty())
                }
            }
        }

        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let actor = PortActor::new(
            Box::new(PartialThenTimeout(PortDriverBase::new(
                "octet_partial",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ))),
            rx,
        );
        let actor_link = actor.link();
        std::thread::spawn(actor.registered());
        let handle = PortHandle::new(
            tx,
            "octet_partial".into(),
            interrupts,
            actor_link,
            std::sync::Arc::new(crate::trace::TraceManager::new()),
        );

        let link = AsynLink {
            port_name: "octet_partial".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_max_size = 40;
        ads.reason_set = true;

        let mut rec = StringinRecord::new("");
        ads.read(&mut rec).unwrap();

        assert_eq!(
            rec.val.as_str_lossy(),
            "abc",
            "the bytes the device did send reach VAL, as C's in-place read does"
        );
        assert_eq!(
            (ads.last_alarm_status, ads.last_alarm_severity),
            (
                epics_base_rs::server::recgbl::alarm_status::TIMEOUT_ALARM,
                epics_base_rs::server::record::AlarmSeverity::Invalid as u16
            ),
            "the failing status still raises TIMEOUT/INVALID (asynStatusToEpicsAlarm)"
        );
    }

    /// Full asynOctetCmdResponse chain through the factory: the literal DRVINFO
    /// command is escape-translated and cached (octet_cmd) with reason_set
    /// pre-set (C useDrvUser=0), so init() skips param resolution and each
    /// process writes the command then reads the reply into VAL (C
    /// `callbackSiCmdResponse`, devAsynOctet.c). Asserted end-to-end on stringin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_factory_writes_escaped_command_then_reads_reply() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::stringin::StringinRecord;

        let (handle, (writes, sequence)) = spawn_cmd_response_port("cmdresp_factory", b"IDN-OK");
        crate::asyn_record::register_port(
            "cmdresp_factory",
            handle,
            &crate::registry::open_owner(),
        )
        .unwrap();

        // The DRVINFO tail "*IDN?\r\n" is the literal command — the "\r\n" is two
        // escape sequences (four chars) in the link, decoded to 0x0D 0x0A.
        let ctx = DeviceSupportContext {
            record_type: "stringin",
            dtyp: "asynOctetCmdResponse",
            inp: "@asyn(cmdresp_factory,0)*IDN?\\r\\n",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = StringinRecord::new("");
        // init() must succeed without the command being a real param — proves the
        // reason_set pre-set (useDrvUser=0) skips drv_user_create.
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        // The escaped command was written once (\r\n → 0x0D 0x0A), before the read.
        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![b"*IDN?\x0d\x0a".to_vec()],
            "the escaped literal command is written once"
        );
        let seq = sequence.lock().unwrap().clone();
        let w = seq.iter().position(|&s| s == "write").unwrap();
        let r = seq.iter().position(|&s| s == "read").unwrap();
        assert!(w < r, "command write must precede the reply read: {seq:?}");
        // C devAsynOctet command-response does plain writeIt → readIt with NO
        // flush — the warm-line bytes are part of the reply, not discarded.
        assert!(
            !seq.contains(&"flush"),
            "CmdResponse must not pre-flush the input buffer: {seq:?}"
        );

        // The reply landed in VAL.
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::String("IDN-OK".into())),
            "the device reply must be stored in VAL"
        );
    }

    /// An escaped NUL in the asynOctetCmdResponse command terminates it: C
    /// initCmdBuffer (devAsynOctet.c:639-641) sets bufLen = strlen(buffer) after
    /// dbTranslateEscape, so the bytes from the NUL onward are never written.
    /// The factory truncates at the first NUL to match the wire bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_command_truncates_at_embedded_nul() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::stringin::StringinRecord;

        let (handle, (writes, _sequence)) = spawn_cmd_response_port("cmdresp_nul", b"R");
        crate::asyn_record::register_port("cmdresp_nul", handle, &crate::registry::open_owner())
            .unwrap();

        // "AB\000CD": dbTranslateEscape yields A B 0x00 C D; C strlen stops at the
        // NUL, so only "AB" reaches the wire.
        let ctx = DeviceSupportContext {
            record_type: "stringin",
            dtyp: "asynOctetCmdResponse",
            inp: "@asyn(cmdresp_nul,0)AB\\000CD",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = StringinRecord::new("");
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![b"AB".to_vec()],
            "the command is truncated at the embedded NUL (C strlen)"
        );
    }

    /// A LEADING-NUL command (raw DRVINFO non-empty, escapes to a leading NUL so
    /// the truncated command is empty) is NOT a misconfiguration: C keys the
    /// empty-reject on strlen(userParam) — the RAW pre-escape DRVINFO
    /// (devAsynOctet.c:631-632) — which is non-empty here, so it computes
    /// bufLen=0 and does a 0-byte writeIt+readIt with NO alarm. base-rs must do
    /// the same, NOT raise LINK_ALARM (which is reserved for a raw-empty DRVINFO).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_leading_nul_command_writes_zero_bytes_no_alarm() {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        use epics_base_rs::server::records::stringin::StringinRecord;

        let (handle, (writes, sequence)) = spawn_cmd_response_port("cmdresp_lnul", b"OK");
        crate::asyn_record::register_port("cmdresp_lnul", handle, &crate::registry::open_owner())
            .unwrap();

        // Raw DRVINFO "\000CD" is non-empty (C strlen != 0 -> no reject); it
        // escapes to [0x00,'C','D'] and truncates at the leading NUL -> empty
        // command -> C writes 0 bytes then reads.
        let ctx = DeviceSupportContext {
            record_type: "stringin",
            dtyp: "asynOctetCmdResponse",
            inp: "@asyn(cmdresp_lnul,0)\\000CD",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = StringinRecord::new("");
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        // A single 0-byte write, then the read — no LINK_ALARM, unlike a
        // raw-empty DRVINFO.
        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![Vec::<u8>::new()],
            "a leading-NUL command writes exactly 0 bytes (C bufLen=0)"
        );
        let seq = sequence.lock().unwrap().clone();
        assert_eq!(
            seq,
            vec!["write", "read"],
            "0-byte write then read: {seq:?}"
        );
        assert_eq!(
            dev.last_alarm().map(|a| (a.stat, a.sevr)),
            None,
            "a leading-NUL command must NOT raise LINK_ALARM (C does not reject it)"
        );
        // The reply still lands in VAL.
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::String("OK".into())));
    }

    /// The asynOctetCmdResponse reply reaches an array-backed (waveform CHAR)
    /// record's VAL, not only a string record — the octet reply→VAL coercion
    /// holds for the CharArray shape. Built with the factory's octet_cmd +
    /// reason_set wiring applied directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_reply_reaches_waveform_char_val() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let (handle, (writes, _)) = spawn_cmd_response_port("cmdresp_wf", b"PONG");
        let link = AsynLink {
            port_name: "cmdresp_wf".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            // Raw DRVINFO must match the cached command: the empty-reject guard
            // keys on a non-empty raw drv_info (C strlen(userParam)).
            drv_info: "PING\\r".to_string(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        // Mirror the factory: cache the escaped command + pre-set reason_set.
        ads.octet_cmd = Some(crate::asyn_record::translate_escape("PING\\r"));
        ads.reason_set = true;
        ads.set_record_info("TEST:WF", ScanType::Passive);

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        ads.read(&mut rec).unwrap();

        assert_eq!(writes.lock().unwrap().clone(), vec![b"PING\x0d".to_vec()]);
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::CharArray(b"PONG".to_vec().into())),
            "reply bytes must populate the waveform CHAR VAL"
        );
    }

    /// The asynOctetCmdResponse reply reaches an lsi (long string in) VAL — the
    /// third C-registered record row (stringin/waveform/lsi). The reply is short,
    /// so no SIZV truncation applies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_reply_reaches_lsi_val() {
        use epics_base_rs::server::records::lsi::LsiRecord;

        let (handle, (writes, _)) = spawn_cmd_response_port("cmdresp_lsi", b"STATUS=OK");
        let link = AsynLink {
            port_name: "cmdresp_lsi".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            // Raw DRVINFO must match the cached command (see the waveform case).
            drv_info: "READ?\\n".to_string(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_cmd = Some(crate::asyn_record::translate_escape("READ?\\n"));
        ads.reason_set = true;
        ads.set_record_info("TEST:LSI", ScanType::Passive);

        let mut rec = LsiRecord::new("");
        ads.read(&mut rec).unwrap();

        assert_eq!(writes.lock().unwrap().clone(), vec![b"READ?\x0a".to_vec()]);
        // lsi backs VAL with a byte buffer, so the reply reads back as a
        // CharArray (matching the waveform CHAR shape, not a fixed String).
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::CharArray(b"STATUS=OK\0".to_vec().into())),
            "reply must populate the lsi VAL"
        );
    }

    /// An octet reply longer than `MAX_STRING_SIZE` reaches the lsi VAL WHOLE.
    /// C `callbackLsiCmdResponse` reads into `plsi->val` bounded by `plsi->sizv`
    /// (devAsynOctet.c:1114-1119) — the 40-byte cap belongs to the DBR_STRING
    /// conversion row, which this path never takes. Delivering the reply as an
    /// `EpicsValue::String` put it through `LsiRecord::put_field`'s DBR_STRING
    /// arm, which truncated at 39 bytes; the reply is bytes, so it is delivered
    /// as bytes and only SIZV bounds it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_long_reply_is_bounded_by_sizv_not_by_string_size() {
        use epics_base_rs::server::records::lsi::LsiRecord;

        // 50 bytes: over MAX_STRING_SIZE - 1 (39), well under the default SIZV.
        let reply: Vec<u8> = (0..50u8).map(|i| b'a' + (i % 26)).collect();
        let (handle, _) = spawn_cmd_response_port("cmdresp_lsi_long", &reply);
        let link = AsynLink {
            port_name: "cmdresp_lsi_long".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "READ?\\n".to_string(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_cmd = Some(crate::asyn_record::translate_escape("READ?\\n"));
        ads.reason_set = true;
        ads.set_record_info("TEST:LSI:LONG", ScanType::Passive);

        let mut rec = LsiRecord::new("");
        ads.read(&mut rec).unwrap();

        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::CharArray([&reply[..], b"\0"].concat().into())),
            "all 50 reply bytes must land — SIZV is the bound, not MAX_STRING_SIZE"
        );
        assert_eq!(
            rec.get_field("LEN"),
            Some(EpicsValue::ULong(reply.len() as u32 + 1)),
            "LEN is nBytesRead + 1 (devAsynOctet.c:1125)"
        );
    }

    /// An asynOctetCmdResponse record with an EMPTY command (DRVINFO escaped to
    /// nothing) is a misconfiguration C rejects at INIT, not at read:
    /// `initCmdBuffer` (devAsynOctet.c:632-637) sets `precord->pact = 1`,
    /// raises LINK_ALARM/INVALID and returns INIT_ERROR. The record is dead —
    /// it performs no I/O because it never processes at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_empty_command_kills_the_record_at_init() {
        use epics_base_rs::server::record::AlarmSeverity;
        use epics_base_rs::server::records::stringin::StringinRecord;

        let (handle, (writes, sequence)) = spawn_cmd_response_port("cmdresp_empty", b"unused");
        let link = AsynLink {
            port_name: "cmdresp_empty".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        // What the factory builds for an empty DRVINFO: octet_cmd = Some(empty).
        ads.octet_cmd = Some(Vec::new());
        ads.reason_set = true;
        ads.set_record_info("TEST:EMPTY", ScanType::Passive);

        let mut rec = StringinRecord::new("");
        assert_eq!(
            ads.init(&mut rec).unwrap(),
            DeviceInitOutcome::dead_with_alarm(
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                AlarmSeverity::Invalid
            ),
            "C initCmdBuffer: recGblSetSevr(LINK_ALARM, INVALID) then pact = 1"
        );

        // No I/O at all — not even an empty write.
        assert!(
            writes.lock().unwrap().is_empty(),
            "empty command must not write to the device"
        );
        assert!(
            sequence.lock().unwrap().is_empty(),
            "empty command must perform no driver I/O"
        );
    }

    // RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed under
    // `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs`.
    /// A table fired on the asynEnum interface (`do_callbacks_enum` for a param
    /// that is not an `Enum`) reaches an `asynInt32` record's property
    /// callback: that callback is the record's asynEnum client, not a second
    /// asynInt32 one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn property_post_receiver_takes_a_table_fired_on_the_enum_interface() {
        use crate::interrupt::InterruptValue;
        use crate::param::{EnumEntry, ParamValue};
        use epics_base_rs::server::records::mbbi::MbbiRecord;

        let init_choices: Arc<[EnumEntry]> =
            Arc::from(vec![enum_entry("OFF", 0, 0), enum_entry("ON", 1, 0)]);
        let mut ads = make_enum_adapter_as(init_choices, "asynInt32");
        let gate = open_gate();
        ads.bind_interrupt_gate(&gate);
        let mut rec = MbbiRecord::new(0);
        ads.init(&mut rec).unwrap();
        let mut rx = ads
            .property_post_receiver()
            .expect("enum record arms the property callback");

        ads.handle.interrupts().notify(InterruptValue {
            reason: ads.reason,
            addr: ads.addr,
            value: ParamValue::Enum {
                index: 0,
                choices: Arc::from(vec![enum_entry("LOW", 0, 0), enum_entry("HIGH", 5, 0)]),
            },
            iface: Some(crate::interfaces::InterfaceType::Enum),
            ..Default::default()
        });

        let post = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("property post must arrive within timeout")
            .expect("channel open");
        let onst = post
            .writes
            .iter()
            .find(|(f, _)| f == "ONST")
            .map(|(_, v)| v);
        assert_eq!(onst, Some(&EpicsValue::String("HIGH".into())));
    }
}
