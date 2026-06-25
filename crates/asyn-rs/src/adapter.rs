use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use crate::error::AsynError;
use crate::interfaces::InterfaceType;
use crate::interrupt::{InterruptFilter, InterruptSubscription};
use crate::port_handle::{AsyncCompletionHandle, PortHandle};
use crate::request::{RequestOp, RequestResult};
use crate::user::AsynUser;

/// Parsed `@asyn(portName, addr, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynLink {
    pub port_name: String,
    pub addr: i32,
    pub timeout: Duration,
    pub drv_info: String,
}

/// Strip an optional sign and detect the C `strtol(.., 0)` base of a
/// numeric link field: a leading `0x`/`0X` selects hexadecimal, a leading
/// `0` (with further digits) octal, otherwise decimal. Returns
/// `(negative, digit_str, radix)`.
///
/// Unlike C — which consumes the longest valid prefix and leaves the rest
/// in `endp` — these require the *whole* (trimmed) token to be a valid
/// number. asyn's comma/space split already isolates each numeric field,
/// so there is no trailing remainder for C's `endp` walk to pick up;
/// rejecting a partly-numeric field is closer to the link parser's intent
/// than silently binding its leading digits.
fn split_base0(tok: &str) -> Option<(bool, &str, u32)> {
    let tok = tok.trim();
    let (neg, rest) = match tok.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, tok.strip_prefix('+').unwrap_or(tok)),
    };
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        // `0x` with no following digits is not a number (C: endp stays at 'x').
        (!hex.is_empty()).then_some((neg, hex, 16))
    } else if rest.len() > 1 && rest.starts_with('0') {
        // Leading `0` + more digits → octal. C `strtol(.., 0)` does NOT
        // recognise a `0o` prefix, so `0o..` falls here and fails the
        // octal digit parse (matching C, which stops at the 'o').
        Some((neg, &rest[1..], 8))
    } else if rest.is_empty() {
        None
    } else {
        // Plain decimal, including a bare "0".
        Some((neg, rest, 10))
    }
}

/// C `strtol(s, _, 0)` for the signed `addr` link field. The `long`→`int`
/// assignment in C truncates on overflow, mirrored here by the `as i32`
/// wrap.
fn strtol_base0_i32(tok: &str) -> Option<i32> {
    let (neg, digits, radix) = split_base0(tok)?;
    let mag = i64::from_str_radix(digits, radix).ok()?;
    Some((if neg { mag.wrapping_neg() } else { mag }) as i32)
}

/// C `strtoul(s, _, 0)` for the unsigned `mask` link field. A leading `-`
/// negates modulo 2^32 (the C standard mandates strtoul negate-then-cast),
/// so `-8` yields `0xFFFFFFF8` — the bit pattern asynInt32 reinterprets as
/// signed nbits via [`Int32Mask::from_nbits`].
fn strtoul_base0_u32(tok: &str) -> Option<u32> {
    let (neg, digits, radix) = split_base0(tok)?;
    let mag = u64::from_str_radix(digits, radix).ok()?;
    Some((if neg { mag.wrapping_neg() } else { mag }) as u32)
}

/// Parse an asyn link string.
///
/// Accepted formats (comma or space delimited, matching C EPICS):
/// - `@asyn(portName) drvInfo`
/// - `@asyn(portName, addr) drvInfo`
/// - `@asyn(portName, addr, timeout) drvInfo`
/// - `@asyn(portName addr) drvInfo`
/// - `@asyn(portName addr timeout) drvInfo`
pub fn parse_asyn_link(s: &str) -> Result<AsynLink, AsynError> {
    let s = s.trim();
    let rest = s
        .strip_prefix("@asyn(")
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("must start with @asyn(: {s}")))?;

    let paren_end = rest
        .find(')')
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("missing closing paren: {s}")))?;

    let args_str = &rest[..paren_end];
    let drv_info = rest[paren_end + 1..].trim().to_string();

    // C EPICS pasynEpicsUtils->parseLink accepts both comma and space as delimiters.
    // Split by comma first; if only one part, try splitting by whitespace.
    let parts: Vec<&str> = if args_str.contains(',') {
        args_str.split(',').map(|p| p.trim()).collect()
    } else {
        args_str.split_whitespace().collect()
    };
    if parts.is_empty() || parts[0].is_empty() {
        return Err(AsynError::InvalidLinkSyntax("portName is required".into()));
    }

    let port_name = parts[0].to_string();
    let addr = if parts.len() > 1 {
        // C asynEpicsUtils.c:114 `strtol(pnext, &endp, 0)` — base auto.
        strtol_base0_i32(parts[1])
            .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("invalid addr: {}", parts[1])))?
    } else {
        0
    };
    let timeout = if parts.len() > 2 {
        let secs: f64 = parts[2]
            .parse()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[2])))?;
        Duration::from_secs_f64(secs)
    } else {
        Duration::from_secs(1)
    };

    Ok(AsynLink {
        port_name,
        addr,
        timeout,
        drv_info,
    })
}

/// Parsed `@asynMask(portName, addr, mask, timeout) drvInfoString` link specification.
#[derive(Debug, Clone)]
pub struct AsynMaskLink {
    pub port_name: String,
    pub addr: i32,
    pub mask: u32,
    pub timeout: Duration,
    pub drv_info: String,
}

/// Parse an asynMask link string.
///
/// Format: `@asynMask(portName, addr, mask[, timeout]) drvInfo`
pub fn parse_asyn_mask_link(s: &str) -> Result<AsynMaskLink, AsynError> {
    let s = s.trim();
    let rest = s
        .strip_prefix("@asynMask(")
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("must start with @asynMask(: {s}")))?;

    let paren_end = rest
        .find(')')
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("missing closing paren: {s}")))?;

    let args_str = &rest[..paren_end];
    let drv_info = rest[paren_end + 1..].trim().to_string();

    let parts: Vec<&str> = args_str.split(',').map(|p| p.trim()).collect();
    if parts.len() < 3 {
        return Err(AsynError::InvalidLinkSyntax(
            "asynMask requires at least 3 arguments: portName, addr, mask".into(),
        ));
    }

    let port_name = parts[0].to_string();
    // C asynEpicsUtils.c:186 `strtol(pnext, &endp, 0)` — base auto.
    let addr = strtol_base0_i32(parts[1])
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("invalid addr: {}", parts[1])))?;

    // C asynEpicsUtils.c:193 `strtoul(pnext, &endp, 0)` — base auto
    // (0x hex, leading-0 octal, else decimal). The resulting 32-bit
    // pattern is reinterpreted per interface: asynInt32 reads it as a
    // signed nbits via `as i32` (a negative count like `-8` parses through
    // strtoul to 0xFFFFFFF8; see `Int32Mask::from_nbits`), asynUInt32Digital
    // as a raw bitmask.
    let mask_str = parts[2];
    let mask = strtoul_base0_u32(mask_str)
        .ok_or_else(|| AsynError::InvalidLinkSyntax(format!("invalid mask: {mask_str}")))?;

    let timeout = if parts.len() > 3 {
        let secs: f64 = parts[3]
            .parse()
            .map_err(|_| AsynError::InvalidLinkSyntax(format!("invalid timeout: {}", parts[3])))?;
        Duration::from_secs_f64(secs)
    } else {
        Duration::from_secs(1)
    };

    Ok(AsynMaskLink {
        port_name,
        addr,
        mask,
        timeout,
        drv_info,
    })
}

/// asynInt32 `@asynMask` bit-count (nbits) configuration.
///
/// For the **asynInt32** interface the third `@asynMask` argument is a
/// signed bit COUNT, not a raw bitmask (C devAsynInt32.c:232-247):
/// a negative count selects *bipolar* handling (sign-extend on read),
/// a positive count *unipolar* (plain low-bit mask). The derived `mask`
/// keeps the low `|nbits|` bits; `sign_bit` is the top of that field;
/// `device_low`/`device_high` are the raw device range used for the
/// LINEAR ESLO/EOFF slope (convertAi:444-451), and they take precedence
/// over the driver's `getBounds` (initAi:822-826).
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
}

/// Adapter bridging an asyn-rs PortDriver to epics-base-rs DeviceSupport.
pub struct AsynDeviceSupport {
    handle: PortHandle,
    addr: i32,
    timeout: Duration,
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
    last_alarm_status: u16,
    last_alarm_severity: u16,
    last_ts: Option<SystemTime>,
    record_name: String,
    scan: ScanType,
    /// Maximum number of array elements for array read operations.
    /// Default: 307200 (enough for 640x480 images).
    max_array_elements: usize,
    /// Buffer cap for `asynOctet` reads, including the trailing NUL.
    /// C asyn devAsynOctet.c:1103 passes `plsi->sizv` as the read
    /// limit for lsi/lso/printf — the byte the driver leaves at
    /// position `sizv` is overwritten with `\0` (line 1124). For
    /// stringin/stringout the C path is the fixed 40-byte record
    /// field; for waveform it uses NELM*FTVL. We pick the per-record
    /// SIZV at init time when available; otherwise fall back to the
    /// stringin-grade 256-byte default.
    octet_max_size: usize,
    /// `Some` ⟹ this is `asynOctetCmdResponse`: an escape-translated literal
    /// command (C `initCmdBuffer`, devAsynOctet.c) written before each read so
    /// the reply lands in VAL. `read_op` emits `OctetWriteRead` instead of a
    /// plain `OctetRead` when present. `None` for ordinary `asynOctetRead`.
    octet_cmd: Option<Vec<u8>>,
    /// If true, read back the current driver value during init (for output records).
    initial_readback: bool,
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
    /// `interruptCallbackAverage` (devAsynInt32.c:870-872 /
    /// devAsynFloat64.c:687-694) + `processAiAverage` (devAsynInt32.c:895-918 /
    /// devAsynFloat64.c:716-735). Only the periodic-SCAN model (mean of all
    /// samples since the last process) is ported; the I/O Intr SVAL-decimation
    /// model is a documented residual.
    average: Option<Arc<AverageState>>,
    /// RAII handle for the averaging synchronous interrupt callback — dropping
    /// unregisters it. Distinct from `interrupt_sub` (the mailbox/value path):
    /// averaging needs every sample, so it registers a synchronous callback
    /// (C `registerInterruptUser`), not a coalescing mailbox subscription.
    average_callback_sub: Option<crate::interrupt::SyncCallbackSubscription>,
    /// RAII interrupt subscription — dropping unsubscribes.
    interrupt_sub: Option<InterruptSubscription>,
    /// Per-record ring buffer of interrupt values, FIFO-ordered. The
    /// I/O Intr forwarding task pushes; `read()` pops the oldest
    /// entry. C parity: `devAsynInt32.c::ringBuffer` (DEFAULT 10,
    /// configurable via `info("asyn:FIFO")`). Overflow policy:
    /// drop-oldest + `overflows++`; the record-process wakeup is
    /// **only** sent on a fresh-entry add (not on overflow
    /// overwrite) so the dbScan queue does not flood.
    interrupt_fifo: Arc<std::sync::Mutex<InterruptFifo>>,
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
    /// True once the `asynFloat64` ai path has written VAL at least once.
    /// The SMOO filter primes on the first read (C `processAi` skips
    /// smoothing while `pr->udf`, devAsynFloat64.c:599); this is the
    /// adapter-side `!udf` for that path, where the adapter is the sole
    /// VAL writer so "have I produced a value yet" is exactly C's prime
    /// signal — and it does not depend on the framework's UDF lifecycle.
    smoo_primed: bool,
    /// Enum state-field family captured at init when the record exposes
    /// ZRST/ZNAM and the driver provided an asynEnum table. `Some` arms the
    /// runtime re-propagation callback ([`Self::property_post_receiver`]);
    /// `None` means no enum table (mirrors C's
    /// `findInterface(asynEnumType) && maxEnums>0` gate).
    enum_shape: Option<EnumRecordShape>,
    /// The driver enum table applied at init — the diff seed for the
    /// runtime callback so a value-only change (enum index moves, choices
    /// unchanged) fires no DBE_PROPERTY post (C's asynEnum callback only
    /// fires on `doCallbacksEnum`, never on an int32 value callback).
    enum_choices: Option<Arc<[crate::param::EnumEntry]>>,
    /// RAII subscription for the runtime enum-table interrupt — dropping
    /// unsubscribes. Distinct from `interrupt_sub` (the value I/O Intr
    /// subscription); C registers the asynEnum callback separately.
    enum_interrupt_sub: Option<InterruptSubscription>,
}

/// Shared state for an averaging (`asynInt32Average` / `asynFloat64Average`)
/// record. The always-on interrupt callback (registered in
/// [`AsynDeviceSupport::io_intr_receiver`]) pushes each driver sample into
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

/// Cached interrupt value with metadata for alarm/timestamp propagation.
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
}

/// Per-record ring buffer for I/O Intr callbacks. Mirrors C
/// `devAsynInt32.c::ringBuffer` + `ringSize` + `ringBufferOverflows`.
struct InterruptFifo {
    entries: std::collections::VecDeque<CachedInterrupt>,
    /// Maximum entries (`asyn:FIFO` info-tag, default
    /// `DEFAULT_RING_BUFFER_SIZE`).
    ring_size: usize,
    /// Running count of drop-oldest overwrites since the last
    /// successful pop. Logged + reset by `pop_callback_value`.
    overflows: u64,
}

/// C `DEFAULT_RING_BUFFER_SIZE` at `devAsynInt32.c:63`.
const DEFAULT_RING_BUFFER_SIZE: usize = 10;

impl InterruptFifo {
    fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(DEFAULT_RING_BUFFER_SIZE),
            ring_size: DEFAULT_RING_BUFFER_SIZE,
            overflows: 0,
        }
    }

    /// Producer-side: push a fresh interrupt entry, dropping the
    /// oldest if full. Returns `true` if this push corresponds to a
    /// *new* entry (and therefore the caller should schedule a record
    /// process); `false` on overflow (no scanIoRequest in C parity).
    fn push_with_overflow(&mut self, entry: CachedInterrupt) -> bool {
        if self.entries.len() >= self.ring_size {
            self.entries.pop_front();
            self.overflows += 1;
            self.entries.push_back(entry);
            false
        } else {
            self.entries.push_back(entry);
            true
        }
    }

    /// Consumer-side: pop the oldest entry. Returns `None` when
    /// empty. The `overflows` counter is reset by the caller via
    /// `take_overflows` so the trace warning fires once per drain
    /// (C `getCallbackValue` behaviour).
    fn pop(&mut self) -> Option<CachedInterrupt> {
        self.entries.pop_front()
    }

    /// Read and clear the overflow counter for a single warning emit.
    fn take_overflows(&mut self) -> u64 {
        std::mem::take(&mut self.overflows)
    }
}

impl AsynDeviceSupport {
    /// Create from a [`PortHandle`].
    pub fn from_handle(handle: PortHandle, link: AsynLink, iface_type: &str) -> Self {
        let iface = InterfaceType::from_asyn_name(iface_type);
        Self {
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
            max_array_elements: 307200,
            octet_max_size: 256,
            octet_cmd: None,
            last_alarm_status: 0,
            last_alarm_severity: 0,
            last_ts: None,
            record_name: String::new(),
            scan: ScanType::Passive,
            initial_readback: false,
            asyn_readback: false,
            write_only: false,
            octet_binary: false,
            average: None,
            average_callback_sub: None,
            interrupt_sub: None,
            interrupt_fifo: Arc::new(std::sync::Mutex::new(InterruptFifo::new())),
            output_callback_pending: false,
            smoo_primed: false,
            enum_shape: None,
            enum_choices: None,
            enum_interrupt_sub: None,
        }
    }

    /// Create with a typed interface from a [`PortHandle`].
    pub fn with_interface_handle(handle: PortHandle, link: AsynLink, iface: InterfaceType) -> Self {
        Self::from_handle(handle, link, iface.asyn_name())
    }

    /// Set the bit mask for UInt32Digital read/write operations.
    ///
    /// For the `asynInt32` interface the same `@asynMask` 3rd arg is a
    /// signed bit COUNT (nbits), not a raw bitmask (C devAsynInt32.c:
    /// 232-247): reinterpret the stored 32-bit pattern as `i32` and derive
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

    /// Override the I/O Intr ring buffer size. The framework
    /// auto-parses `info("asyn:FIFO", "<n>")` via
    /// [`Self::apply_record_info`]; this manual setter exists for
    /// callers outside the IocApplication wiring. C parity:
    /// `devAsynInt32.c::createRingBuffer` at line 354-365 — sets
    /// `pPvt->ringSize` to `atoi(info)` or `DEFAULT_RING_BUFFER_SIZE`
    /// when the info tag is absent.
    pub fn set_fifo_size(&mut self, n: usize) {
        let mut g = self.interrupt_fifo.lock().unwrap();
        g.ring_size = n.max(1);
        // Drop any over-capacity entries already buffered so the new
        // limit takes effect immediately. Counts the truncation as
        // overflows so the trace warning fires.
        while g.entries.len() > g.ring_size {
            g.entries.pop_front();
            g.overflows = g.overflows.saturating_add(1);
        }
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
    use epics_base_rs::server::record::AlarmSeverity;
    match e {
        AsynError::Status { status, .. } => {
            asyn_status_to_alarm_with_default(*status, default_stat)
        }
        // Non-status asyn errors take C's asynError/default branch.
        _ => (default_stat, AlarmSeverity::Invalid as u16),
    }
}

/// [`asyn_error_to_alarm_with_default`] with C's input default (`READ_ALARM`).
fn asyn_error_to_alarm(e: &AsynError) -> (u16, u16) {
    asyn_error_to_alarm_with_default(e, epics_base_rs::server::recgbl::alarm_status::READ_ALARM)
}

/// Convert an asyn ParamValue to an EpicsValue.
fn param_value_to_epics_value(pv: &crate::param::ParamValue) -> Option<EpicsValue> {
    use crate::param::ParamValue;
    match pv {
        ParamValue::Int32(v) => Some(EpicsValue::Long(*v)),
        ParamValue::Int64(v) => Some(EpicsValue::Double(*v as f64)),
        ParamValue::Float64(v) => Some(EpicsValue::Double(*v)),
        ParamValue::Octet(s) => Some(EpicsValue::String(s.clone().into())),
        ParamValue::UInt32Digital(v) => Some(EpicsValue::Long(*v as i32)),
        ParamValue::Enum { index, .. } => Some(EpicsValue::Enum(*index as u16)),
        ParamValue::Int8Array(a) => {
            Some(EpicsValue::CharArray(a.iter().map(|&x| x as u8).collect()))
        }
        ParamValue::Int16Array(a) => Some(EpicsValue::ShortArray(a.to_vec())),
        ParamValue::Int32Array(a) => Some(EpicsValue::LongArray(a.to_vec())),
        ParamValue::Int64Array(a) => {
            Some(EpicsValue::LongArray(a.iter().map(|&x| x as i32).collect()))
        }
        ParamValue::Float32Array(a) => Some(EpicsValue::FloatArray(a.to_vec())),
        ParamValue::Float64Array(a) => Some(EpicsValue::DoubleArray(a.to_vec())),
        _ => None,
    }
}

/// Convert a native-typed array interrupt to the element type of the consuming
/// record's asyn array interface.
///
/// C `NDPluginStdArrays::processCallbacks` fires an interrupt on every one of
/// the six array interfaces, each running `pNDArrayPool->convert(pArray, ...,
/// signedType)` so a record on any interface receives its own type-converted
/// copy (NDPluginStdArrays.cpp:169-197). The Rust runtime carries a single
/// native-typed array per param; this routes it through the same per-interface
/// `convert` the polled `readArray` path uses ([`AsynDeviceSupport::result_to_value`]),
/// so an I/O Intr waveform whose FTVL differs from the array's native element
/// type still gets correctly-converted data each frame instead of the raw
/// native array. Returns `None` for non-array interfaces (scalar records keep
/// [`param_value_to_epics_value`]).
fn convert_param_array_to_iface(
    iface_type: &str,
    pv: &crate::param::ParamValue,
) -> Option<EpicsValue> {
    use crate::param::ParamValue;
    // `src as $t` is exactly the polled converter: integer->integer is a
    // truncating C cast, float->integer saturates, and casts to float round —
    // matching `copy_ccast`/`copy_convert` in the plugin runtime.
    macro_rules! cast_vec {
        ($t:ty) => {{
            match pv {
                ParamValue::Int8Array(a) => a.iter().map(|&x| x as $t).collect::<Vec<$t>>(),
                ParamValue::Int16Array(a) => a.iter().map(|&x| x as $t).collect(),
                ParamValue::Int32Array(a) => a.iter().map(|&x| x as $t).collect(),
                ParamValue::Int64Array(a) => a.iter().map(|&x| x as $t).collect(),
                ParamValue::Float32Array(a) => a.iter().map(|&x| x as $t).collect(),
                ParamValue::Float64Array(a) => a.iter().map(|&x| x as $t).collect(),
                _ => return None,
            }
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

/// Bridges async `AsyncCompletionHandle` to epics-base-rs `WriteCompletion`.
struct AsynAsyncWriteCompletion {
    handle: parking_lot::Mutex<Option<AsyncCompletionHandle>>,
}

impl WriteCompletion for AsynAsyncWriteCompletion {
    fn wait(&self, timeout: Duration) -> CaResult<()> {
        if let Some(h) = self.handle.lock().take() {
            match h.wait_blocking(timeout) {
                Ok(_) => Ok(()),
                Err(e) => Err(CaError::Protocol(e.to_string())),
            }
        } else {
            Ok(())
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
    /// C parity for `devAsynInt32.c::initAi` (lines 821-828) +
    /// `convertAi` (lines 437-454): query the driver's int32 / int64
    /// bounds, then compute `ESLO = (EGUF - EGUL) / (high - low)` and
    /// `EOFF = (high*EGUL - low*EGUF) / (high - low)` and write
    /// them on the record. Caller has already verified the record
    /// exposes ESLO and the interface is `asynInt32` / `asynInt64`.
    fn apply_linear_eslo_eoff(&self, record: &mut dyn Record) {
        // C devAsynInt32.c:822-826 — the nbits-derived deviceLow/High (set
        // when @asynMask gives a bit count) take precedence; `getBounds` is
        // the fallback only when no nbits was specified.
        let (low, high) = if let Some(m) = self.int32_mask {
            (m.device_low as f64, m.device_high as f64)
        } else {
            let op = if self.iface_type == "asynInt64" {
                RequestOp::GetBoundsInt64
            } else {
                RequestOp::GetBoundsInt32
            };
            let user = AsynUser::new(self.reason)
                .with_addr(self.addr)
                .with_timeout(self.timeout);
            let result = match self.handle.submit_blocking(op, user) {
                Ok(r) => r,
                Err(_) => return,
            };
            match result.bounds {
                Some((l, h)) => (l as f64, h as f64),
                None => return,
            }
        };
        // C parity: `if (deviceHigh != deviceLow)` (convertAi:444).
        // A degenerate driver that reports 0,0 (the "I don't know"
        // sentinel — matches the early-out at initAi:824) leaves the
        // existing ESLO/EOFF untouched.
        if (high - low).abs() < f64::EPSILON {
            return;
        }
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

    /// Build a `RequestOp` for reading the current interface type.
    fn read_op(&self) -> Option<RequestOp> {
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
                    data: cmd.clone(),
                    buf_size: self.octet_max_size,
                    flush: false,
                },
                None => RequestOp::OctetRead {
                    buf_size: self.octet_max_size,
                },
            }),
            "asynUInt32Digital" => Some(RequestOp::UInt32DigitalRead { mask: self.mask }),
            "asynEnum" => Some(RequestOp::EnumRead),
            "asynInt8Array" => Some(RequestOp::Int8ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt16Array" => Some(RequestOp::Int16ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt32Array" => Some(RequestOp::Int32ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynInt64Array" => Some(RequestOp::Int64ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynFloat32Array" => Some(RequestOp::Float32ArrayRead {
                max_elements: self.max_array_elements,
            }),
            "asynFloat64Array" => Some(RequestOp::Float64ArrayRead {
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

    /// Extract an EpicsValue from a RequestResult based on interface type.
    fn result_to_value(&self, result: &RequestResult) -> Option<EpicsValue> {
        match self.iface_type.as_str() {
            "asynInt32" => result
                .int_val
                .map(|v| EpicsValue::Long(self.apply_int32_mask(v))),
            "asynInt64" => result.int64_val.map(|v| EpicsValue::Double(v as f64)),
            "asynFloat64" => result.float_val.map(EpicsValue::Double),
            "asynOctet" => result.data.as_ref().map(|d| {
                let n = result.nbytes.min(d.len());
                EpicsValue::String(String::from_utf8_lossy(&d[..n]).into_owned().into())
            }),
            "asynUInt32Digital" => result.uint_val.map(|v| EpicsValue::Long(v as i32)),
            "asynEnum" => result.enum_index.map(|v| EpicsValue::Enum(v as u16)),
            "asynInt8Array" => result
                .int8_array
                .clone()
                .map(|v| EpicsValue::CharArray(v.iter().map(|&x| x as u8).collect())),
            "asynInt16Array" => result.int16_array.clone().map(EpicsValue::ShortArray),
            "asynInt32Array" => result.int32_array.clone().map(EpicsValue::LongArray),
            "asynInt64Array" => result
                .int64_array
                .clone()
                .map(|v| EpicsValue::LongArray(v.iter().map(|&x| x as i32).collect())),
            "asynFloat32Array" => result.float32_array.clone().map(EpicsValue::FloatArray),
            "asynFloat64Array" => result.float64_array.clone().map(EpicsValue::DoubleArray),
            _ => None,
        }
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
        let nord = match record.get_field("NORD") {
            Some(EpicsValue::Long(n)) => (n.max(0) as usize).min(data.len()),
            _ => data.len(),
        };
        Some(RequestOp::OctetWrite {
            data: data[..nord].to_vec(),
        })
    }

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
            ("asynOctet", EpicsValue::String(s)) => Some(RequestOp::OctetWrite {
                data: s.as_bytes().to_vec(),
            }),
            ("asynOctet", EpicsValue::CharArray(data)) => {
                // Trim trailing nulls (waveform FTVL=CHAR pads to NELM)
                let len = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                Some(RequestOp::OctetWrite {
                    data: data[..len].to_vec(),
                })
            }
            // Coerce numeric types to octet (e.g. longout writing to NDArrayPort string param)
            ("asynOctet", v) => {
                let s = format!("{v}");
                Some(RequestOp::OctetWrite {
                    data: s.as_bytes().to_vec(),
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
            ("asynInt8Array", EpicsValue::CharArray(data)) => Some(RequestOp::Int8ArrayWrite {
                data: data.iter().map(|&x| x as i8).collect(),
            }),
            ("asynInt16Array", EpicsValue::ShortArray(data)) => {
                Some(RequestOp::Int16ArrayWrite { data: data.clone() })
            }
            ("asynInt32Array", EpicsValue::LongArray(data)) => {
                Some(RequestOp::Int32ArrayWrite { data: data.clone() })
            }
            ("asynInt64Array", EpicsValue::LongArray(data)) => Some(RequestOp::Int64ArrayWrite {
                data: data.iter().map(|&x| x as i64).collect(),
            }),
            ("asynFloat32Array", EpicsValue::FloatArray(data)) => {
                Some(RequestOp::Float32ArrayWrite { data: data.clone() })
            }
            ("asynFloat64Array", EpicsValue::DoubleArray(data)) => {
                Some(RequestOp::Float64ArrayWrite { data: data.clone() })
            }
            _ => None,
        }
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
    ///   `processBi`), mbbi (mask/shift/state index, `processMbbi`), mbbiDirect
    ///   (mask/shift/bits, `processMbbiDirect`). The hook is the *device-distinct*
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
                // `processAo` (devAsynFloat64.c:646-649). Returning `true`
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
                // mbbiDirect's mask/shift/bits (inputs). `apply_raw_readback`
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
                // MASK/SHFT/state table), so the one dispatch covers both the
                // asynInt32 and the asynUInt32Digital readback. ai routes via
                // the ESLO branch below; longin declines (default `false`).
                if record.apply_raw_readback(raw) {
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
        let _ = record.set_val(val);
        true
    }

    /// The ASLO/AOFF/SMOO arithmetic of C `devAsynFloat64::processAi`
    /// (devAsynFloat64.c:595-602). `ASLO`/`AOFF`/`SMOO` are read live so a
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
    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            match self.handle.drv_user_create_blocking(&self.drv_info) {
                Ok(reason) => {
                    self.reason = reason;
                }
                Err(e) => {
                    // Param not found — this record has no corresponding driver param.
                    eprintln!(
                        "[asyn] init FAILED: port='{}' drv_info='{}' err={e}",
                        self.handle.port_name(),
                        self.drv_info
                    );
                    self.reason_set = false;
                    return Ok(());
                }
            }
            self.reason_set = true;
        }

        // Read NELM from the record to set max_array_elements for array reads.
        if let Some(EpicsValue::Long(nelm)) = record.get_field("NELM") {
            if nelm > 0 {
                self.max_array_elements = nelm as usize;
            }
        }

        // Read SIZV from lsi/lso/printf records to size the asynOctet
        // read buffer. C parity: devAsynOctet.c:1103 passes
        // `plsi->sizv` to initCommon as the per-record buffer size;
        // the read path (line 1117-1124) then writes up to sizv-1
        // bytes and stuffs `\0` at the boundary. For stringin /
        // stringout (no SIZV) we keep the 256-byte default. SIZV is
        // DBF_USHORT (lsiRecord/lsoRecord/printfRecord.dbd.pod), so the
        // field reads back as an unsigned 16-bit value.
        if let Some(EpicsValue::UShort(sizv)) = record.get_field("SIZV") {
            if sizv > 0 {
                self.octet_max_size = sizv as usize;
            }
        }

        // asynUInt32Digital MASK/SHFT propagation. C devAsynUInt32Digital.c
        // (init paths for mbbi/mbbo/mbbiDirect/mbboDirect at lines 881,
        // 925, 1010, 1054) sets:
        //
        //     pr->mask = pPvt->mask;
        //     pr->shft = computeShift(pPvt->mask);
        //
        // so the record's standard RVAL→VAL conversion shifts the
        // masked bits down to bit 0. Without this, a link like
        // `@asynMask(port,0,0xFF00) BITS` reads RVAL with bits 8-15
        // set but the record's shft stays 0 and VAL ends up
        // 65280-shaped instead of 0-255. Apply this regardless of
        // mask value — a fully-set 0xFFFFFFFF mask shifts by 0
        // (no-op) which is the correct contract for "every bit".
        if self.iface_type == "asynUInt32Digital" && self.mask != 0 {
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
        // when NOBT==0. mbbiRecord/mbboRecord init (mbbiRecord.c:128-130) sets
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

        // ai/ao LINEAR ESLO/EOFF wiring.
        //
        // C devAsynInt32.c::initAi (line 822-828) / initAo / initAiAverage:
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
            self.apply_linear_eslo_eoff(record);
        }

        // Driver enum-string table -> record state fields. C asyn int32/uint32
        // device support (devAsynInt32.c::initCommon:297-324,
        // devAsynUInt32Digital.c:547-601) queries the driver's asynEnum
        // interface and `setEnums` the strings/values/severities onto the
        // record (ZRST/ZRVL/ZRSV… for mbbi/mbbo, ZNAM/ONAM… for bi/bo). The
        // record family is identified by the state fields it exposes (ZRST or
        // ZNAM); the EnumRead itself only returns a table when the driver
        // provides one (an Enum param or an overridden read_enum), mirroring
        // C's `findInterface(asynEnumType) != NULL` gate — a driver without
        // an enum table makes EnumRead fail and the record keeps its .db
        // strings.
        if let Some(shape) = EnumRecordShape::of_record(record) {
            let user = AsynUser::new(self.reason)
                .with_addr(self.addr)
                .with_timeout(self.timeout);
            if let Ok(result) = self.handle.submit_blocking(RequestOp::EnumRead, user) {
                if let Some(entries) = result.enum_entries {
                    self.apply_enum_table(record, &entries);
                    // Capture the shape + table so the runtime callback
                    // (property_post_receiver) can re-propagate on change.
                    // Only set when the driver actually provided a table —
                    // C registers callbackEnum only inside the
                    // `findInterface(asynEnumType) && maxEnums>0` block.
                    self.enum_shape = Some(shape);
                    self.enum_choices = Some(entries);
                }
            }
        }

        if self.initial_readback {
            if let Some(op) = self.read_op() {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                if let Ok(result) = self.handle.submit_blocking(op, user) {
                    // Seed the output record's value only on a successful read,
                    // mirroring C initAo/initLongout/initMbbo: `if (status ==
                    // asynSuccess) { pao->rval = value } return
                    // INIT_DO_NOT_CONVERT` (devAsynInt32.c:955-959, :1080-1082).
                    // A non-success initial read leaves the .db default — the
                    // init-time member of the aux_status value-store family
                    // (process-time members gated in the ring/polled/average
                    // paths).
                    if result.aux_status == crate::error::AsynStatus::Success {
                        if let Some(val) = self.result_to_value(&result) {
                            // ao seeds VAL from the raw readback via its
                            // `raw -> eng` inverse: C `initAo` sets `rval=value`
                            // and returns INIT_OK so aoRecord runs the readback
                            // convert (devAsynInt32.c:947-957). longout/mbbo
                            // have no such inverse (VAL == raw / state-map) and
                            // decline the hook (default `false`). An asynFloat64
                            // ao seeds VAL with the forward ASLO/AOFF scaling
                            // (C `initAo`, devAsynFloat64.c:627-629) via the
                            // float64 readback hook.
                            let seeded = match val {
                                EpicsValue::Long(raw) => record.apply_raw_readback(raw),
                                EpicsValue::Double(raw) => record.apply_float64_readback(raw),
                                _ => false,
                            };
                            if !seeded {
                                let _ = record.set_val(val);
                            }
                        }
                    }
                }
            }
        }

        // Mode 1 averaging: snapshot the SVAL decimation count so the first
        // decimation period uses the configured value before the first process
        // refreshes it (C reads pai->sval live; devAsynInt32.c:674).
        if self.scan == ScanType::IoIntr {
            self.refresh_average_decimation_threshold(record);
        }
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        if !self.reason_set {
            return Ok(DeviceReadOutcome::ok());
        }

        // asynOctetCmdResponse misconfiguration: C initCmdBuffer
        // (devAsynOctet.c:631-637) rejects the command outright —
        // recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM) + INIT_ERROR — when, and
        // ONLY when, the RAW userParam (the pre-escape DRVINFO) is empty:
        // `strlen(pPvt->userParam) == 0`. The translated byte length (bufLen) is
        // computed later (:641) and may be 0 for a non-empty DRVINFO that escapes
        // to a leading NUL — that case is NOT rejected; C does a 0-byte
        // writeIt+readIt. So gate the reject on the raw DRVINFO, not on the
        // post-truncation command: a Some(empty) octet_cmd from a leading-NUL
        // command falls through to a 0-byte OctetWriteRead. octet_cmd is None for
        // a plain asynOctetRead, so this only fires for a CmdResponse record.
        if self.octet_cmd.is_some() && self.drv_info.is_empty() {
            self.last_alarm_status = epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
            self.last_alarm_severity = epics_base_rs::server::record::AlarmSeverity::Invalid as u16;
            return Ok(DeviceReadOutcome::ok());
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
            if let Some(val) = self.device_output_value(&*record) {
                let op = if self.octet_binary {
                    self.binary_write_op(&*record, &val)
                } else {
                    self.write_op(&val)
                };
                if let Some(op) = op {
                    let user = AsynUser::new(self.reason)
                        .with_addr(self.addr)
                        .with_timeout(self.timeout);
                    // Apply the write result's alarm, mirroring the read path
                    // (and C callbackWfWrite/WriteBinary: writeIt -> result.status
                    // -> finish recGblSetSevr, devAsynOctet.c:1071-1076,1086-1091).
                    // A successful write carries NO_ALARM (clears any prior); a
                    // failure maps via asyn_error_to_alarm. Without this the
                    // write-only path swallowed every write failure silently.
                    // This is an output (isOutput=1) record, so a generic
                    // transport failure raises WRITE_ALARM, not READ_ALARM
                    // (C processCommon: isOutput ? WRITE_ALARM : READ_ALARM,
                    // devAsynOctet.c:806-807; partial write recGblSetSevr
                    // WRITE_ALARM, :685).
                    match self.handle.submit_blocking(op, user) {
                        Ok(result) => {
                            self.last_alarm_status = result.alarm_status;
                            self.last_alarm_severity = result.alarm_severity;
                        }
                        Err(e) => {
                            let (alarm_status, alarm_severity) = asyn_error_to_alarm_with_default(
                                &e,
                                epics_base_rs::server::recgbl::alarm_status::WRITE_ALARM,
                            );
                            self.last_alarm_status = alarm_status;
                            self.last_alarm_severity = alarm_severity;
                        }
                    }
                }
            }
            return Ok(DeviceReadOutcome::ok());
        }

        // Mode 1 averaging (SCAN="I/O Intr"): the decimated mean is delivered
        // through the IoIntr ring — `io_intr_receiver` decimates every
        // round(SVAL) samples into `interrupt_fifo` and wakes the record (C
        // `interruptCallbackAverage` isIOIntrScan branch, devAsynInt32.c:
        // 673-702). Refresh the live SVAL threshold, then fall through to the
        // ring-pop path below which drains the decimated value (C
        // `processAiAverage` `getCallbackValue` branch, :895-898). Do NOT drain
        // the running mean here — that is the Mode 2 periodic model.
        if self.average.is_some() && self.scan == ScanType::IoIntr {
            self.refresh_average_decimation_threshold(record);
        }
        // Mode 2 averaging (periodic SCAN): the always-on interrupt callback
        // (io_intr_receiver) has been accumulating samples into the SumAverager
        // since iocInit. On each periodic record process, drain the arithmetic
        // mean since the last process and reset — C `processAiAverage`
        // (devAsynInt32.c:895-918, devAsynFloat64.c:716-735). Reuses the normal
        // ai store paths: int32 routes the rounded mean to RVAL (the ai record's
        // convert applies ESLO/EOFF), float64 sets VAL directly (ASLO/AOFF/SMOO
        // applied in-dset) — averaging only changes the *source* of the raw
        // value, exactly as CmdResponse reused the octet reply→VAL.
        else if let Some(acc) = self.average.clone() {
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
                        // devAsynFloat64.c:736-754). `computed()` skips the
                        // store so RVAL/VAL keep their previous value; the
                        // mapped READ/TIMEOUT/...@INVALID alarm above stands.
                        return Ok(DeviceReadOutcome::computed());
                    }
                    // Transport success: store the mean (C return 0/2).
                    let skip_convert = self.store_read_value(record, val);
                    return Ok(if skip_convert {
                        DeviceReadOutcome::computed()
                    } else {
                        DeviceReadOutcome::ok()
                    });
                }
                None => {
                    // No samples since the last process. C `processAiAverage`
                    // sets UDF_ALARM/INVALID and returns -2 (VAL untouched) —
                    // devAsynInt32.c:900-904, devAsynFloat64.c:721-725.
                    // `computed()` skips the RVAL→VAL convert so VAL keeps its
                    // previous value (no store_read_value call). NOTE: C also
                    // sets the record's `udf=1` boolean; device support has no
                    // channel to set that field here — the UDF_ALARM/INVALID
                    // alarm is raised, the `udf` boolean is the documented gap.
                    self.last_alarm_status = epics_base_rs::server::recgbl::alarm_status::UDF_ALARM;
                    self.last_alarm_severity =
                        epics_base_rs::server::record::AlarmSeverity::Invalid as u16;
                    return Ok(DeviceReadOutcome::computed());
                }
            }
        }

        // For I/O Intr records, pop the oldest entry from the ring
        // buffer. C parity: `devAsynInt32.c::getCallbackValue` —
        // returns the next FIFO entry, logs+resets the overflow
        // counter on consume. An `asyn:READBACK` output record consumes
        // from the same ring: the framework calls `read()` on it only on a
        // driver-callback cycle (`process_record_readback`), so popping
        // the FIFO here is the output-record analogue of
        // `processBo`'s `getCallbackValue` readback branch.
        if self.scan == ScanType::IoIntr || self.asyn_readback {
            // The record reached its read stage, so this driver-callback
            // cycle processed: clear the armed flag (C `processBo` clears
            // `newOutputCallbackValue` when it runs). A survived flag after
            // the cycle means the PACT guard bailed before here, which
            // `reconcile_readback_callback` then repairs.
            self.output_callback_pending = false;
            let (entry, overflows) = {
                let mut fifo = self.interrupt_fifo.lock().unwrap();
                (fifo.pop(), fifo.take_overflows())
            };
            if overflows > 0 {
                tracing::warn!(
                    target: "asyn_rs::adapter",
                    port = %self.handle.port_name(),
                    record = %self.record_name,
                    overflows = overflows,
                    "ring buffer overflows (C asyn ASYN_TRACE_WARNING)"
                );
            }
            let mut skip_convert = true;
            if let Some(ci) = entry {
                // Direction-aware default for the transport-status mapping: C
                // `asynStatusToEpicsAlarm` takes READ_ALARM for input device
                // support (processAi, devAsynInt32.c:844) and WRITE_ALARM for
                // scalar output readback (processBo/Lo/Mbbo, :1201). Array dsets
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
                // — C sets `time` even on a transport error (devAsynXXXArray.c:
                // 327) and always recGblSetSevr's the mapped alarm.
                let (status, severity) = resolve_intr_alarm(
                    (ci.alarm_status, ci.alarm_severity),
                    ci.aux_status,
                    default_stat,
                );
                self.last_alarm_status = status;
                self.last_alarm_severity = severity;
                self.last_ts = Some(ci.timestamp);
                // C gates the value store on the transport status:
                // `if (result.status == asynSuccess) { pr->rval = … } else
                // return -1` (processAi devAsynInt32.c:844-855, processBo
                // :1201-1204), and the array process() copies bptr/nord only
                // `if (rp->status == asynSuccess)` (devAsynXXXArray.cpp:317).
                // On a transport error keep the prior value (skip the store);
                // `skip_convert` stays true so the record skips the RVAL→VAL
                // convert and keeps its previous value = C return -1.
                if ci.aux_status == crate::error::AsynStatus::Success {
                    // Array interrupts carry the native element type; convert to
                    // this record's interface type so a mismatched FTVL gets the
                    // same per-type `convert` the polled path applies (C fires
                    // all six array interfaces, NDPluginStdArrays.cpp:169-197).
                    // Scalar interfaces fall back to the verbatim mapping.
                    let val = convert_param_array_to_iface(&self.iface_type, &ci.value)
                        .or_else(|| param_value_to_epics_value(&ci.value));
                    if let Some(val) = val {
                        skip_convert = self.store_read_value(record, val);
                    }
                }
            }
            // Honor store_read_value's skip-convert decision: computed()
            // (skip RVAL→VAL convert, C return 2) for paths that produced
            // the final VAL themselves, ok() (run the record convert, C
            // return 0) for the asynInt32 ai path that routed raw to RVAL.
            return Ok(if skip_convert {
                DeviceReadOutcome::computed()
            } else {
                DeviceReadOutcome::ok()
            });
        }

        let mut skip_convert = true;
        if let Some(op) = self.read_op() {
            let user = AsynUser::new(self.reason)
                .with_addr(self.addr)
                .with_timeout(self.timeout);
            match self.handle.submit_blocking(op, user) {
                Ok(result) => {
                    // Gate the value store on the device read status, mirroring
                    // C processAi: `if (result.status == asynSuccess) { pr->rval
                    // = value } else { return -1 }` keeps the prior value on a
                    // non-success read (devAsynInt32.c:848-855). The alarm/time
                    // are applied unconditionally (C maps the alarm and
                    // recGblSetSevr's it before the status gate, :844-847) — same
                    // store-only gate as the I/O Intr ring.
                    if result.aux_status == crate::error::AsynStatus::Success {
                        if let Some(val) = self.result_to_value(&result) {
                            skip_convert = self.store_read_value(record, val);
                        }
                    }
                    self.last_alarm_status = result.alarm_status;
                    self.last_alarm_severity = result.alarm_severity;
                    self.last_ts = result.timestamp;
                }
                Err(e) => {
                    // Convert asyn error to EPICS alarm (C parity: asynStatusToEpicsAlarm)
                    let (alarm_status, alarm_severity) = asyn_error_to_alarm(&e);
                    self.last_alarm_status = alarm_status;
                    self.last_alarm_severity = alarm_severity;
                }
            }
        }
        // computed()/ok() per store_read_value (see the I/O Intr branch above).
        Ok(if skip_convert {
            DeviceReadOutcome::computed()
        } else {
            DeviceReadOutcome::ok()
        })
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if !self.reason_set {
            return Ok(());
        }
        if let Some(val) = self.device_output_value(record) {
            if let Some(op) = self.write_op(&val) {
                let user = AsynUser::new(self.reason)
                    .with_addr(self.addr)
                    .with_timeout(self.timeout);
                self.handle
                    .submit_blocking(op, user)
                    .map_err(asyn_to_ca_error)?;
            }
        }
        Ok(())
    }

    fn dtyp(&self) -> &str {
        &self.iface_type
    }

    fn last_alarm(&self) -> Option<(u16, u16)> {
        if self.last_alarm_status == 0 && self.last_alarm_severity == 0 {
            None
        } else {
            Some((self.last_alarm_status, self.last_alarm_severity))
        }
    }

    fn last_timestamp(&self) -> Option<SystemTime> {
        self.last_ts
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        self.record_name = name.to_string();
        self.scan = scan;
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
        if let Some(raw) = info.get("asyn:INITIAL_READBACK") {
            self.set_initial_readback(parse_info_bool(raw));
        }
        // C parity: `info("asyn:FIFO", "<n>")` at
        // devAsynInt32.c:361-362 — `atoi(sizeString)` overrides
        // DEFAULT_RING_BUFFER_SIZE (10). C uses raw `atoi`, which
        // returns 0 for unparseable input; we mirror that by
        // ignoring zero / negative values so the default isn't
        // accidentally clobbered by a typo.
        if let Some(raw) = info.get("asyn:FIFO") {
            if let Ok(n) = raw.trim().parse::<i64>() {
                if n > 0 {
                    self.set_fifo_size(n as usize);
                }
            }
        }
    }

    fn write_begin(
        &mut self,
        record: &mut dyn Record,
    ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        // Same raw-output anchor as the synchronous write() — the async path
        // (blocking ports, e.g. motors) must not bypass the RVAL/OVAL anchor.
        let val = match self.device_output_value(record) {
            Some(v) => v,
            None => return Ok(None),
        };
        let op = match self.write_op(&val) {
            Some(op) => op,
            None => return Ok(None),
        };
        let user = AsynUser::new(self.reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout);

        // For non-blocking ports, use synchronous submit to match C EPICS behavior:
        // the write completes within the same dbProcess call, so CP chain targets
        // see the updated value immediately. This prevents actor channel overflow
        // and stale reads during fast motor moves.
        if !self.handle.can_block() {
            let _ = self
                .handle
                .submit_blocking(op, user)
                .map_err(asyn_to_ca_error)?;
            return Ok(None); // completed synchronously, no async completion needed
        }

        let completion = self.handle.try_submit(op, user).map_err(asyn_to_ca_error)?;
        Ok(Some(Box::new(AsynAsyncWriteCompletion {
            handle: parking_lot::Mutex::new(Some(completion)),
        })))
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        // Activate the driver-callback path for either:
        //   1. records with `SCAN="I/O Intr"` (legacy behaviour), OR
        //   2. records flagged via `set_asyn_readback(true)` (asyn
        //      upstream PRs #60 / #208 — output records that follow
        //      driver-side changes regardless of SCAN).
        if !self.reason_set {
            return None;
        }

        // Averaging device support (asynInt32Average / asynFloat64Average):
        // register an ALWAYS-ON accumulating callback regardless of SCAN (C
        // enables the averaging callback unconditionally, devAsynInt32.c:385-386).
        // It is SYNCHRONOUS (register_sync_callback / C registerInterruptUser),
        // not a mailbox subscription: averaging must observe every sample, and
        // the mailbox coalesces rapid updates to the latest — which would drop
        // samples and corrupt the mean.
        if let Some(acc) = self.average.clone() {
            let filter = InterruptFilter {
                reason: Some(self.reason),
                addr: Some(self.addr),
                uint32_mask: None,
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
            if self.scan == ScanType::IoIntr {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                let fifo = self.interrupt_fifo.clone();
                let sub = self
                    .handle
                    .interrupts()
                    .register_sync_callback(filter, move |iv| {
                        use std::sync::atomic::Ordering;
                        let sample = match &iv.value {
                            crate::param::ParamValue::Int32(v) => {
                                Some(int32_mask.map_or(*v, |m| m.apply(*v)) as f64)
                            }
                            crate::param::ParamValue::Float64(v) => Some(*v),
                            _ => None,
                        };
                        let Some(s) = sample else {
                            return;
                        };
                        // C: numAverage++; sum += value (devAsynInt32.c:665-666).
                        acc.averager.push(s);
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
                        };
                        // C: ring full → evict oldest + overflow, NO
                        // scanIoRequest; fresh add → scanIoRequest
                        // (devAsynInt32.c:688-701).
                        let was_fresh = {
                            let mut g = fifo.lock().unwrap();
                            g.push_with_overflow(entry)
                        };
                        // try_send: the callback runs inline in the driver
                        // notify() and must not block; a full wakeup channel
                        // means a process is already pending and will drain the
                        // ring (C does not re-request when one is pending).
                        if was_fresh {
                            let _ = tx.try_send(());
                        }
                    });
                self.average_callback_sub = Some(sub);
                return Some(rx);
            }

            // Mode 2 — periodic SCAN: accumulate every sample; the periodic
            // record process drains the running mean in read(). The callback
            // OR-accumulates the transport status and stashes the last sample's
            // EPICS alarm (devAsynInt32.c:705-707, devAsynFloat64.c:516-518) —
            // once any sample is non-success the period stays a transport error
            // (asynSuccess == 0, so the OR is sticky). Return None: no
            // per-callback reprocess wakeup (the record scans on its own period).
            let sub = self
                .handle
                .interrupts()
                .register_sync_callback(filter, move |iv| {
                    let sample = match &iv.value {
                        crate::param::ParamValue::Int32(v) => {
                            Some(int32_mask.map_or(*v, |m| m.apply(*v)) as f64)
                        }
                        crate::param::ParamValue::Float64(v) => Some(*v),
                        _ => None,
                    };
                    if let Some(s) = sample {
                        acc.averager.push(s);
                        let mut st = acc.status.lock().unwrap();
                        st.last_alarm = (iv.alarm_status, iv.alarm_severity);
                        if iv.aux_status != crate::error::AsynStatus::Success {
                            st.aux_error = Some(iv.aux_status);
                        }
                    }
                });
            self.average_callback_sub = Some(sub);
            return None;
        }

        if self.scan != ScanType::IoIntr && !self.asyn_readback {
            return None;
        }

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
            addr: Some(self.addr),
            uint32_mask: if is_uint32 { Some(mask) } else { None },
        };

        let (sub, mut intr_rx) = self.handle.interrupts().register_interrupt_user(filter);
        self.interrupt_sub = Some(sub);

        // Bridge mailbox-based InterruptReceiver to the mpsc<()> wakeup channel
        // consumed by setup_io_intr(). The mailbox already coalesces intermediate
        // updates, so no data is lost even if the record processes slowly.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let fifo = self.interrupt_fifo.clone();
        tokio::spawn(async move {
            while let Some(iv) = intr_rx.recv().await {
                // C parity (asynPortDriver.cpp:729 + devAsynUInt32Digital.c:464):
                // deliver `mask & value` for UInt32Digital so the I/O-Intr value
                // matches the polled read (RequestOp::UInt32DigitalRead { mask }).
                // interruptCallbackInput stores the already-masked value in the
                // ring buffer; mirror that here. For asynInt32 apply the
                // @asynMask nbits mask + sign-extend (devAsynInt32.c:537-540),
                // matching the polled `result_to_value` path. Other values
                // pass through.
                let value = match iv.value {
                    crate::param::ParamValue::UInt32Digital(v) if is_uint32 => {
                        crate::param::ParamValue::UInt32Digital(v & mask)
                    }
                    crate::param::ParamValue::Int32(v) => {
                        crate::param::ParamValue::Int32(int32_mask.map_or(v, |m| m.apply(v)))
                    }
                    other => other,
                };
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
                };
                // C parity (devAsynInt32.c:564-576):
                //   - On overflow (ring full), drop oldest +
                //     overflows++ and DO NOT call scanIoRequest.
                //     The already-pending process will pick up the
                //     newer tail; a duplicate request would just
                //     flood dbScan.
                //   - On normal add, request the record to process.
                let was_fresh_add = {
                    let mut g = fifo.lock().unwrap();
                    g.push_with_overflow(entry)
                };
                if was_fresh_add && tx.send(()).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }

    /// An `asyn_readback`-flagged record follows driver-side changes via the
    /// interrupt callback regardless of its `SCAN` (upstream PRs #60 / #208).
    /// Decouple its poll-feedback wiring from the `SCAN` menu so the callback
    /// processes it even when `SCAN != "I/O Intr"`. Plain `SCAN="I/O Intr"`
    /// records (`asyn_readback == false`) keep the SCAN-gated behaviour.
    ///
    /// Averaging records (`average.is_some()`) also need the driver-callback
    /// path wired regardless of SCAN — the accumulating interrupt must run on
    /// every driver sample while the record scans periodically. C enables the
    /// averaging callback always, independent of SCAN (devAsynInt32.c:385-386).
    /// `io_intr_receiver` returns `None` for the average case, so this only
    /// arms the accumulating subscription; it spawns no reprocess task (the
    /// record scans on its own period, not per callback).
    fn io_intr_scan_independent(&self) -> bool {
        self.asyn_readback || self.average.is_some()
    }

    fn arm_readback_callback(&mut self) {
        // C `devAsynInt32.c::outputCallbackCallback` sets
        // `newOutputCallbackValue = 1` immediately before `dbProcess`. Only
        // the output-readback path (asyn:READBACK, PRs #60 / #208) carries
        // the C `newOutputCallbackValue` contract; input I/O-Intr records use
        // the scan/RPRO reprocess model (`processCommon`), never the discard
        // fallback, so they are left unarmed and `reconcile` is a no-op there.
        if self.asyn_readback {
            self.output_callback_pending = true;
        }
    }

    fn reconcile_readback_callback(&mut self) {
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
            if let Ok(mut fifo) = self.interrupt_fifo.lock() {
                let _ = fifo.pop();
            }
            self.output_callback_pending = false;
        }
    }

    fn property_post_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::Receiver<Vec<(String, EpicsValue)>>> {
        // Runtime asynEnum table re-propagation. C devAsynInt32.c registers a
        // per-record enum callback (callbackEnum, :711-762) that re-applies
        // `setEnums` + `db_post_events(DBE_PROPERTY)` whenever the driver
        // changes the table via `doCallbacksEnum`. Gate exactly as C's
        // `findInterface(asynEnumType) && maxEnums>0`: only a record that
        // exposed an enum state family AND received a driver table at init
        // (so `enum_shape` is set) arms the callback. Independent of SCAN —
        // a property post is not a value scan, so it never processes the
        // record.
        let shape = self.enum_shape?;
        if !self.reason_set {
            return None;
        }

        // Subscribe to the enum param's interrupts. In asyn-rs the enum value
        // (index) and the enum table (choices) live on one `ParamValue::Enum`
        // param, so both a value change and a `set_enum_choices` change arrive
        // here; C separates them across the asynInt32 and asynEnum interfaces.
        // The bridge below recovers that separation by posting only when the
        // *choices* differ from the last-applied table (seeded with the init
        // table) — so an int32 value change fires no DBE_PROPERTY, matching C.
        let filter = InterruptFilter {
            reason: Some(self.reason),
            addr: Some(self.addr),
            uint32_mask: None,
        };
        let (sub, mut intr_rx) = self.handle.interrupts().register_interrupt_user(filter);
        self.enum_interrupt_sub = Some(sub);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut last_choices = self.enum_choices.clone();
        tokio::spawn(async move {
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
                let fields = enum_table_fields(shape, choices);
                if tx.send(fields).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

// ===== Universal asyn device support =====

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
    dtyp.to_string()
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
    // Try @asyn() link in INP or OUT
    let (link_str, is_output) = if ctx.out.contains("@asyn") || ctx.out.contains("@asynMask") {
        (ctx.out, true)
    } else if ctx.inp.contains("@asyn") || ctx.inp.contains("@asynMask") {
        // asynOctetWrite / asynOctetWriteBinary use the INP field for output
        // (C waveform-output-via-INP convention; initWfWrite / initWfWriteBinary
        // both pass &pwf->inp with isOutput=1, devAsynOctet.c:1065,1080).
        let is_write_dtyp = ctx.dtyp == "asynOctetWrite" || ctx.dtyp == "asynOctetWriteBinary";
        (ctx.inp, is_write_dtyp)
    } else {
        return None;
    };

    // Parse the link
    let link = if link_str.contains("@asynMask") {
        let ml = parse_asyn_mask_link(link_str).ok()?;
        AsynLink {
            port_name: ml.port_name,
            addr: ml.addr,
            timeout: ml.timeout,
            drv_info: ml.drv_info,
        }
    } else {
        parse_asyn_link(link_str).ok()?
    };

    // Look up port in global registry
    let entry = crate::asyn_record::get_port(&link.port_name)?;

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

    // asynInt32Average / asynFloat64Average: averaging device support. C
    // `initAiAverage` (devAsynInt32.c:760+, devAsynFloat64.c) registers an
    // always-on accumulating interrupt; the periodic record process drains
    // the mean. The DRVINFO is a real param (useDrvUser=1), so `reason_set`
    // is left false and `init()` runs `drv_user_create` as for a plain
    // asynInt32/asynFloat64 ai. The accumulating subscription is registered
    // in `io_intr_receiver` (which `io_intr_scan_independent` enables here).
    if ctx.dtyp == "asynInt32Average" || ctx.dtyp == "asynFloat64Average" {
        adapter.average = Some(Arc::new(AverageState::new()));
    }

    if is_output {
        if ctx.dtyp == "asynOctetWrite" || ctx.dtyp == "asynOctetWriteBinary" {
            // asynOctetWrite / asynOctetWriteBinary are write-only — no reads
            // allowed. Reading would replace the waveform CharArray with a String,
            // breaking element_count.
            adapter.write_only = true;
            // asynOctetWriteBinary writes the full NORD bytes with NO NUL-trim
            // (C callbackWfWriteBinary, devAsynOctet.c:1086-1091), unlike
            // asynOctetWrite which trims at the first NUL (my_strnlen, :1071-1076).
            adapter.octet_binary = ctx.dtyp == "asynOctetWriteBinary";
        } else {
            // Output records: read back current driver value on init.
            // Mirrors C `initAo` / `initBo` / `initLongout` / `initMbbo`
            // which call `pasynManager->queueRequest(... ASYN_INIT ...)`
            // to pull the driver's current value into the record before
            // record processing starts.
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

    // UInt32Digital: apply mask
    if link_str.contains("@asynMask") {
        if let Ok(ml) = parse_asyn_mask_link(link_str) {
            adapter = adapter.with_mask(ml.mask);
        }
    }

    Some(Box::new(adapter))
}

/// Register universal asyn device support on an IocApplication.
///
/// This is the Rust equivalent of C EPICS's standard asyn device support
/// registration. Call this BEFORE registering plugin or driver-specific
/// factories so they take precedence (dynamic factories chain last-registered-first).
pub fn register_asyn_device_support(
    app: epics_base_rs::server::ioc_app::IocApplication,
) -> epics_base_rs::server::ioc_app::IocApplication {
    app.register_dynamic_device_support(universal_asyn_factory)
}

/// IocBuilder companion to [`register_asyn_device_support`] —
/// installs the universal asyn factory on the pure-Rust build path
/// (added `register_dynamic_device_support` to IocBuilder).
/// Without this helper, callers using `IocBuilder` instead of
/// `IocApplication` would have to wire `universal_asyn_factory`
/// manually; that asymmetry is exactly what `register_asyn_device_support`
/// hides for the IocApplication path.
pub fn register_asyn_device_support_for_builder(
    builder: epics_base_rs::server::ioc_builder::IocBuilder,
) -> epics_base_rs::server::ioc_builder::IocBuilder {
    builder.register_dynamic_device_support(universal_asyn_factory)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(link.timeout, Duration::from_secs_f64(1.0));
        assert_eq!(link.drv_info, "TEMPERATURE");
    }

    #[test]
    fn test_parse_port_only() {
        let link = parse_asyn_link("@asyn(port1) PARAM").unwrap();
        assert_eq!(link.port_name, "port1");
        assert_eq!(link.addr, 0);
        assert_eq!(link.timeout, Duration::from_secs(1));
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
        assert_eq!(link.timeout, Duration::from_secs_f64(0.5));
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
        assert_eq!(link.timeout, Duration::from_secs_f64(1.5));
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
        assert_eq!(link.timeout, Duration::from_secs_f64(2.0));
        assert_eq!(link.drv_info, "BITS");
    }

    #[test]
    fn test_parse_mask_link_no_timeout() {
        let link = parse_asyn_mask_link("@asynMask(port1, 0, 255) BITS").unwrap();
        assert_eq!(link.mask, 255);
        assert_eq!(link.timeout, Duration::from_secs(1));
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

    #[test]
    fn strtol_base0_covers_hex_octal_decimal_sign() {
        assert_eq!(strtol_base0_i32("0x10"), Some(16));
        assert_eq!(strtol_base0_i32("010"), Some(8));
        assert_eq!(strtol_base0_i32("10"), Some(10));
        assert_eq!(strtol_base0_i32("0"), Some(0));
        assert_eq!(strtol_base0_i32("-0x10"), Some(-16));
        assert_eq!(strtol_base0_i32("+7"), Some(7));
        // Invalid digit for the detected base / non-numeric → reject.
        assert_eq!(strtol_base0_i32("08"), None);
        assert_eq!(strtol_base0_i32("0o20"), None);
        assert_eq!(strtol_base0_i32("abc"), None);
        assert_eq!(strtol_base0_i32("0x"), None);
    }

    #[test]
    fn strtoul_base0_negation_wraps_mod_2_32() {
        // C strtoul negates-then-casts: -8 => 0xFFFFFFF8.
        assert_eq!(strtoul_base0_u32("-8"), Some(0xFFFF_FFF8));
        assert_eq!(strtoul_base0_u32("0xFF00"), Some(0xFF00));
        assert_eq!(strtoul_base0_u32("010"), Some(8));
        assert_eq!(strtoul_base0_u32("255"), Some(255));
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
            let interrupts = Arc::new(InterruptManager::new(256));
            let (tx, _rx) = tokio::sync::mpsc::channel(256);
            (PortHandle::new(tx, name.into(), interrupts), _rx)
        };
        let link = |port: &str| AsynLink {
            port_name: port.into(),
            addr: 0,
            timeout: Duration::from_secs(1),
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
    fn result_to_value_masks_and_sign_extends_int32() {
        // The polled read path (result_to_value) must apply the nbits mask +
        // sign-extend (C processCallbackInput, devAsynInt32.c:485-488).
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let handle = PortHandle::new(tx, "p".into(), interrupts);
        let link = AsynLink {
            port_name: "p".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: String::new(),
        };
        // bipolar 8-bit (@asynMask ... -8): raw 0xFF reads back as -1.
        let ads =
            AsynDeviceSupport::from_handle(handle, link, "asynInt32").with_mask((-8i32) as u32);
        let result = RequestResult::int32_read(0xFF);
        assert_eq!(ads.result_to_value(&result), Some(EpicsValue::Long(-1)));
    }

    #[test]
    fn array_interrupt_converts_to_record_interface_type() {
        use crate::param::ParamValue;
        // An F64-native array delivered on the I/O Intr path must be converted to
        // each consuming interface's element type (C fires all six array
        // interfaces type-converted, NDPluginStdArrays.cpp:169-197).
        let f64arr = ParamValue::Float64Array(std::sync::Arc::from([1.7f64, 2.9, -3.1].as_slice()));

        assert_eq!(
            convert_param_array_to_iface("asynInt16Array", &f64arr),
            Some(EpicsValue::ShortArray(vec![1, 2, -3])),
        );
        assert_eq!(
            convert_param_array_to_iface("asynFloat64Array", &f64arr),
            Some(EpicsValue::DoubleArray(vec![1.7, 2.9, -3.1])),
        );

        // Integer narrowing is a truncating C cast, matching the polled ccast
        // path (40000 as i16 wraps to -25536), not a saturating convert.
        let i32arr = ParamValue::Int32Array(std::sync::Arc::from([40000i32].as_slice()));
        assert_eq!(
            convert_param_array_to_iface("asynInt16Array", &i32arr),
            Some(EpicsValue::ShortArray(vec![-25536])),
        );

        // asynInt8Array goes through i8 then reinterprets to u8 (300.0 -> i8
        // saturates to 127 -> u8 127), matching result_to_value's CharArray map.
        let over = ParamValue::Float64Array(std::sync::Arc::from([300.0f64].as_slice()));
        assert_eq!(
            convert_param_array_to_iface("asynInt8Array", &over),
            Some(EpicsValue::CharArray(vec![127])),
        );

        // Scalar interfaces are not array interfaces: returns None so the caller
        // falls back to the verbatim mapping.
        assert_eq!(convert_param_array_to_iface("asynFloat64", &f64arr), None);
    }

    use crate::error::AsynResult;
    use crate::interrupt::{InterruptManager, InterruptValue};
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;
    use std::sync::Arc;

    struct TestPort {
        base: PortDriverBase,
    }
    impl TestPort {
        fn new() -> Self {
            let mut base = PortDriverBase::new("test", 1, PortFlags::default());
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
            let mut base = PortDriverBase::new("test_bounds", 1, PortFlags::default());
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
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-bounds-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test_bounds".into(), interrupts);
        let link = AsynLink {
            port_name: "test_bounds".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.set_record_info("TEST:AI", ScanType::Passive);
        ads
    }

    fn make_adapter(scan: ScanType) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-adapter-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);

        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
        ads.set_record_info("TEST:REC", scan);
        ads
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
        driver
            .base
            .params
            .set_param_status(0, 0, status, 0, 0)
            .unwrap();
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-seeded-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynInt32");
        ads.set_record_info("TEST:SEED", ScanType::Passive);
        ads.reason_set = true; // reason defaults to 0 = VAL; skip init's drv_user_create
        ads
    }

    /// Build an averaging (`asynInt32Average` / `asynFloat64Average`) adapter:
    /// a plain ai adapter on the base interface with `average` set, on a
    /// periodic (non-IoIntr) SCAN. The accumulating interrupt is armed by
    /// calling `io_intr_receiver()` (which `io_intr_scan_independent` would
    /// gate in production); the port is never read by the average path.
    fn make_average_adapter(iface: &str) -> AsynDeviceSupport {
        let driver = TestPort::new();
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-average-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, iface);
        ads.average = Some(Arc::new(AverageState::new()));
        ads.set_record_info("TEST:AVG", ScanType::Passive);
        ads
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
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-u32-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "VAL".into(),
        };
        let mut ads =
            AsynDeviceSupport::from_handle(handle, link, "asynUInt32Digital").with_mask(mask);
        ads.set_record_info("TEST:U32", ScanType::IoIntr);
        ads
    }

    /// Poll the interrupt FIFO until an entry arrives (bridge runs on a
    /// spawned task), returning its value. Panics if none arrives.
    async fn await_fifo_value(
        fifo: &Arc<std::sync::Mutex<InterruptFifo>>,
    ) -> crate::param::ParamValue {
        for _ in 0..200 {
            if let Some(entry) = fifo.lock().unwrap().pop() {
                return entry.value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("interrupt value never reached FIFO");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_intr_uint32_masks_delivered_value_to_record_mask() {
        // C uint32Callback delivers `pInterrupt->mask & value`
        // (asynPortDriver.cpp:729); the @asynMask=0x0F record must see
        // a 0xFF param value masked to 0x0F, matching the polled read.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

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
        // C uint32Callback fires only when `pInterrupt->mask &
        // interruptMask` (asynPortDriver.cpp:720): a change confined to
        // bits outside the @asynMask=0x0F record (changed=0xF0) must be
        // gated out, while an overlapping change still arrives.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_uint32_io_intr_adapter(0x0F);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

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
        // Guard: the default 0xFFFFFFFF mask must NOT gate a non-UInt32
        // interrupt (uint32_changed_mask == 0). An asynInt32 I/O-Intr
        // record must receive its interrupt unchanged.
        use epics_base_rs::server::records::longin::LonginRecord;
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut rec = LonginRecord::new(0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let fifo = ads.interrupt_fifo.clone();
        let _rx = ads.io_intr_receiver().expect("io intr receiver for IoIntr");

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
                });
        }
        let len = |ads: &AsynDeviceSupport| ads.interrupt_fifo.lock().unwrap().entries.len();

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

    /// A driver that does NOT implement getBounds reports low=high=0
    /// (C asynInt32Base.c:99), so convertAi skips the LINEAR ESLO/EOFF
    /// computation (devAsynInt32.c:444). TestPort overrides no bounds,
    /// exercising the PortDriver default; ESLO/EOFF must be left at the
    /// record's pre-set values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_linear_skip_when_driver_uses_default_bounds() {
        let mut ads = make_adapter(ScanType::Passive); // TestPort: default bounds
        use epics_base_rs::server::records::ai::AiRecord;
        let mut rec = AiRecord::new(0.0);
        rec.eguf = 10.0;
        rec.egul = 0.0;
        rec.linr = 2; // LINEAR
        rec.eslo = 123.456;
        rec.eoff = 42.0;
        ads.init(&mut rec).unwrap();
        assert!(
            (rec.eslo - 123.456).abs() < 1e-9,
            "ESLO must be untouched when the driver relies on the default (0,0) bounds: got {}",
            rec.eslo
        );
        assert!(
            (rec.eoff - 42.0).abs() < 1e-9,
            "EOFF must be untouched when the driver relies on the default (0,0) bounds: got {}",
            rec.eoff
        );
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
            !outcome.did_compute,
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
        assert!(!outcome.did_compute);
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
            outcome.did_compute,
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        let mut rec = AiRecord::new(0.0);
        rec.linr = 0; // NO_CONVERSION: VAL == RVAL after convert
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        // Average arms the accumulating interrupt but returns no reprocess
        // wakeup — the record scans periodically (periodic-SCAN model).
        assert!(
            ads.io_intr_receiver().is_none(),
            "averaging records get no per-callback reprocess channel"
        );

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
            !outcome.did_compute,
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        assert!(ads.io_intr_receiver().is_none());

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
            outcome.did_compute,
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
        assert!(ads.io_intr_receiver().is_none());

        // No samples notified → the accumulator is empty.
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute,
            "zero-samples skips the convert so VAL is not overwritten from RVAL"
        );
        assert_eq!(
            ads.last_alarm(),
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        let mut rec = AiRecord::new(0.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        assert!(ads.io_intr_receiver().is_none());

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
            ads.last_alarm(),
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        let mut rec = AiRecord::new(42.0); // pre-existing VAL
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        assert!(ads.io_intr_receiver().is_none());

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
            outcome.did_compute,
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
            ads.last_alarm(),
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
        assert!(outcome2.did_compute);
        assert_eq!(
            ads.last_alarm(),
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynFloat64");
        let mut rec = AiRecord::new(42.0);
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        assert!(ads.io_intr_receiver().is_none());

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
            ads.last_alarm(),
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
    /// resets the accumulator. Unlike Mode 2, the averaging adapter returns a
    /// per-decimation reprocess wakeup (`io_intr_receiver` → `Some`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_mode1_io_intr_decimates_every_sval_samples_into_ring() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
        ads.set_record_info("TEST:AVG", ScanType::IoIntr); // Mode 1
        let mut rec = AiRecord::new(0.0);
        rec.sval = 4.0; // numToAverage = round(4.0) = 4
        ads.init(&mut rec).unwrap();
        let reason = ads.reason;
        let interrupts = ads.handle.interrupts().clone();
        let acc = ads.average.clone().unwrap();
        let fifo = ads.interrupt_fifo.clone();

        // Mode 1 delivers a reprocess wakeup channel (Mode 2 returns None).
        assert!(
            ads.io_intr_receiver().is_some(),
            "Mode 1 (SCAN=I/O Intr) averaging delivers a per-decimation reprocess wakeup"
        );

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

    /// C floors the decimation count at 1: `numToAverage = (int)(sval+0.5); if
    /// (numToAverage < 1) numToAverage = 1` (devAsynInt32.c:674-675). With
    /// SVAL=0 every single sample decimates (the mean of one sample is itself).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn average_mode1_sval_below_one_floors_to_one_sample() {
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
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
        use epics_base_rs::server::records::ai::AiRecord;
        let mut ads = make_average_adapter("asynInt32");
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
                if let Some(e) = fifo.lock().unwrap().pop() {
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

        let mut base = PortDriverBase::new("test_enum", 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();
        base.set_enum_choices_param(0, 0, choices).unwrap();
        let driver = EnumPort { base };

        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-enum-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test_enum".into(), interrupts);
        let link = AsynLink {
            port_name: "test_enum".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: "MODE".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynEnum");
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

    /// C `devAsynInt32::initCommon` (297-324) reads the driver's asynEnum
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

    /// Runtime asynEnum re-propagation: after init captures the table, a
    /// driver enum-table change delivered through the interrupt path must
    /// surface on the `property_post_receiver` channel as the new mbb field
    /// block — but an interrupt carrying the SAME choices (a value-only
    /// change) must fire NO post (C's asynEnum callback only runs on a table
    /// change, never on an int32 value callback).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn property_post_receiver_emits_only_on_table_change() {
        use crate::interrupt::InterruptValue;
        use crate::param::{EnumEntry, ParamValue};
        use epics_base_rs::server::records::mbbi::MbbiRecord;

        let init_choices: Arc<[EnumEntry]> =
            Arc::from(vec![enum_entry("OFF", 0, 0), enum_entry("ON", 1, 0)]);
        let mut ads = make_enum_adapter(init_choices.clone());
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

        let fields = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("property post must arrive within timeout")
            .expect("channel open");
        let get = |name: &str| {
            fields
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

    /// Every getBounds default returns (0, 0), matching C
    /// asynInt32Base.c:99 / asynInt64Base.c:99 (`*low = *high = 0`).
    #[test]
    fn get_bounds_defaults_match_c_low_high_zero() {
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
        assert_eq!(TestPort::new().get_bounds_int32(&u).unwrap(), (0, 0));
        assert_eq!(TestPort::new().get_bounds_int64(&u).unwrap(), (0, 0));
    }

    // --- asyn:FIFO ring buffer ---

    fn intr_entry(v: i32, t_ms: u64) -> CachedInterrupt {
        CachedInterrupt {
            value: crate::param::ParamValue::Int32(v),
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(t_ms),
            alarm_status: 0,
            alarm_severity: 0,
            aux_status: crate::error::AsynStatus::Success,
        }
    }

    #[test]
    fn fifo_default_size_matches_c_constant() {
        // C devAsynInt32.c:63 → DEFAULT_RING_BUFFER_SIZE = 10.
        let f = InterruptFifo::new();
        assert_eq!(f.ring_size, 10);
        assert!(f.entries.is_empty());
        assert_eq!(f.overflows, 0);
    }

    #[test]
    fn fifo_push_pop_fifo_order() {
        let mut f = InterruptFifo::new();
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        let popped: Vec<_> = std::iter::from_fn(|| f.pop())
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![1, 2, 3], "FIFO order, not LIFO");
        assert_eq!(f.take_overflows(), 0);
    }

    #[test]
    fn fifo_overflow_drops_oldest_and_counts() {
        // C parity: devAsynInt32.c:566-571 — when ringHead wraps onto
        // ringTail, advance ringTail (drop oldest) + overflows++.
        let mut f = InterruptFifo::new();
        f.ring_size = 3;
        assert!(f.push_with_overflow(intr_entry(1, 1)));
        assert!(f.push_with_overflow(intr_entry(2, 2)));
        assert!(f.push_with_overflow(intr_entry(3, 3)));
        // Now full: every additional push must be reported as overflow.
        assert!(!f.push_with_overflow(intr_entry(4, 4)));
        assert!(!f.push_with_overflow(intr_entry(5, 5)));
        assert_eq!(f.overflows, 2);
        // Buffer now holds [3, 4, 5] — oldest two dropped.
        let popped: Vec<_> = std::iter::from_fn(|| f.pop())
            .map(|c| match c.value {
                crate::param::ParamValue::Int32(v) => v,
                _ => panic!(),
            })
            .collect();
        assert_eq!(popped, vec![3, 4, 5]);
    }

    #[test]
    fn fifo_take_overflows_resets() {
        let mut f = InterruptFifo::new();
        f.ring_size = 1;
        f.push_with_overflow(intr_entry(1, 1));
        f.push_with_overflow(intr_entry(2, 2)); // overflow
        f.push_with_overflow(intr_entry(3, 3)); // overflow
        assert_eq!(f.take_overflows(), 2);
        // Second call must return 0 — counter was reset.
        assert_eq!(f.take_overflows(), 0);
    }

    #[test]
    fn set_fifo_size_truncates_existing_entries() {
        let mut ads = make_adapter(ScanType::IoIntr);
        {
            let mut g = ads.interrupt_fifo.lock().unwrap();
            g.ring_size = 10;
            g.push_with_overflow(intr_entry(1, 1));
            g.push_with_overflow(intr_entry(2, 2));
            g.push_with_overflow(intr_entry(3, 3));
            g.push_with_overflow(intr_entry(4, 4));
        }
        // Shrink — must drop the 2 oldest and count them as overflows.
        ads.set_fifo_size(2);
        let g = ads.interrupt_fifo.lock().unwrap();
        assert_eq!(g.entries.len(), 2);
        assert_eq!(g.overflows, 2);
    }

    #[test]
    fn apply_record_info_parses_asyn_fifo() {
        let mut ads = make_adapter(ScanType::IoIntr);
        let mut info = std::collections::HashMap::new();
        info.insert("asyn:FIFO".to_string(), "32".to_string());
        ads.apply_record_info(&info);
        assert_eq!(ads.interrupt_fifo.lock().unwrap().ring_size, 32);

        // C atoi("garbage") = 0 → keep default (we additionally
        // require n > 0).
        info.insert("asyn:FIFO".to_string(), "garbage".to_string());
        ads.apply_record_info(&info);
        assert_eq!(
            ads.interrupt_fifo.lock().unwrap().ring_size,
            32,
            "non-numeric must not clobber size"
        );

        // Negative / zero is rejected too.
        info.insert("asyn:FIFO".to_string(), "0".to_string());
        ads.apply_record_info(&info);
        assert_eq!(ads.interrupt_fifo.lock().unwrap().ring_size, 32);
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

    /// C parity: devAsynOctet initCommon passes `plsi->sizv` as the
    /// asynOctet read-buffer size; an lsi record with a non-default
    /// SIZV must produce read ops sized accordingly, not the fixed
    /// 256-byte stringin default.
    #[test]
    fn octet_buffer_picks_up_sizv_from_record() {
        let mut ads = make_adapter(ScanType::Passive);
        // The adapter only routes asynOctet through octet_max_size,
        // so re-target it.
        ads.set_iface_type("asynOctet");
        // Default (no init yet) is the stringin fallback.
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
    /// at devAsynOctet.c:357 — info tag overrides the per-record
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
            });
        }
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(7)));
        assert_eq!(
            ads.last_alarm(),
            Some((3, 1)),
            "IoIntr read must surface the driver alarm carried by the ring entry"
        );
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
        assert_eq!(ads.last_alarm(), None, "clean entry => no alarm");
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
        assert_eq!(ads.last_alarm(), None);
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Long(11)),
            "transport error must keep the prior value (C return -1), not store 99"
        );
        assert_eq!(
            ads.last_alarm(),
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm(),
            Some((1, 3)),
            "input asynError default => READ_ALARM(1)/INVALID(3)"
        );
    }

    /// Contrast with the input case: a scalar `asyn:READBACK` OUTPUT record takes
    /// WRITE_ALARM for the `asynError`/unknown default (processBo, devAsynInt32.c:
    /// 1201). Same shared ring read, direction picked from `asyn_readback`.
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm(),
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            ads.last_alarm(),
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
            ads.last_alarm(),
            Some((9, 3)),
            "Disconnected maps to COMM_ALARM(9)/INVALID(3)"
        );
        assert_eq!(
            ads.interrupt_fifo.lock().unwrap().entries.len(),
            1,
            "the error entry must be consumed, leaving exactly the clean follow-up"
        );
        // Second read: the clean entry is the value stored.
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::Long(50)));
        assert_eq!(ads.last_alarm(), None);
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
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-arr-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test".into(), interrupts);
        let link = AsynLink {
            port_name: "test".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.val(), Some(EpicsValue::LongArray(vec![1, 2, 3])));

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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(
            rec.val(),
            Some(EpicsValue::LongArray(vec![1, 2, 3])),
            "transport error must keep the prior array (C devAsynXXXArray.cpp:317)"
        );
        assert_eq!(
            ads.last_alarm(),
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
            ads.last_alarm(),
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
        assert_eq!(ads.last_alarm(), None, "success read raises no alarm");
    }

    /// Init-time output readback (asyn:READBACK) seeds the record's value only on
    /// a successful read, mirroring C initAo/initLongout/initMbbo: `if (status ==
    /// asynSuccess) { pao->rval = value } return INIT_DO_NOT_CONVERT`
    /// (devAsynInt32.c:955-959, :1080-1082). A non-success initial read leaves the
    /// .db default — the init-time member of the aux_status value-store family.
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
        rec.eslo = 2.0;
        rec.eoff = 10.0;
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute,
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute,
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "uint32digital mbbo readback computed");
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "bo readback computed");
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(
            outcome.did_compute,
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "uint32digital bi readback computed");
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
            });
        ads.read(&mut rec).unwrap();
        assert_eq!(rec.rval, 0, "out-of-mask bits masked away");
        assert_eq!(rec.val, 0, "masked-to-zero raw → val 0");
    }

    /// An `asynUInt32Digital` I/O Intr `mbbiDirect` input (its only dset,
    /// `asynMbbiDirectUInt32Digital`): `processMbbiDirect` sets
    /// `rval = value & mask` (devAsynUInt32Digital.c:1031); mbbiDirectRecord
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "mbbiDirect readback computed");
        assert_eq!(rec.rval, 0x3C, "RVAL keeps the masked (unshifted) raw");
        assert_eq!(
            rec.val, 0x0F,
            "VAL = (raw & mask) >> SHFT, not the raw 0x3C"
        );
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "mbbi readback computed");
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
            });
        let outcome = ads.read(&mut rec).unwrap();
        assert!(outcome.did_compute, "mbbi readback computed");
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
    /// init only recomputes (1<<NOBT)-1 when MASK==0, mbbiRecord.c:128-130).
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
                let mut base = PortDriverBase::new("test_f64", 1, PortFlags::default());
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
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-f64-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, "test_f64".into(), interrupts);
        let link = AsynLink {
            port_name: "test_f64".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
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
    /// (devAsynFloat64.c:646-649). ao carries no SMOO so it never enters the ai
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
            outcome.did_compute,
            "float64 ao readback produces VAL itself → computed"
        );
        assert_eq!(
            rec.val(),
            Some(EpicsValue::Double(21.0)),
            "VAL = raw*ASLO + AOFF = 10*2 + 1 = 21"
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
    /// `processMbbo` writes `pr->rval` (devAsynInt32.c:1332). RVAL is ULong and
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
        // asynOctetWriteBinary likewise — write-only/no-NUL-trim is carried by
        // octet_binary, not a distinct interface.
        assert_eq!(normalize_asyn_dtyp("asynOctetWriteBinary"), "asynOctet");
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
        fn drv_user_create(&self, _drv_info: &str) -> AsynResult<usize> {
            Ok(0)
        }
        fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
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
            Ok(())
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
            base: PortDriverBase::new(name, 1, PortFlags::default()),
            writes: writes.clone(),
            fail,
        };
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(port), rx);
        std::thread::Builder::new()
            .name("binwrite-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, name.into(), interrupts);
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
        crate::asyn_record::register_port(
            "binwrite_wb",
            handle,
            Arc::new(crate::trace::TraceManager::new()),
        );

        let ctx = DeviceSupportContext {
            dtyp: "asynOctetWriteBinary",
            inp: "@asyn(binwrite_wb,0)REG",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        // NORD = 3, with an interior NUL; asynOctetWrite would trim at the NUL.
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02]))
            .unwrap();
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![vec![0x01, 0x00, 0x02]],
            "binary write must send the full NORD bytes, interior NUL included"
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
        crate::asyn_record::register_port(
            "binwrite_text",
            handle,
            Arc::new(crate::trace::TraceManager::new()),
        );

        let ctx = DeviceSupportContext {
            dtyp: "asynOctetWrite",
            inp: "@asyn(binwrite_text,0)REG",
            out: "",
        };
        let mut dev = universal_asyn_factory(&ctx).expect("factory builds the device");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02]))
            .unwrap();
        dev.init(&mut rec).unwrap();
        dev.read(&mut rec).unwrap();

        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![vec![0x01]],
            "text write must trim at the first NUL"
        );
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
            timeout: Duration::from_secs(1),
            drv_info: "REG".into(),
        };
        let ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x01, 0x00, 0x02]))
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
            timeout: Duration::from_secs(1),
            drv_info: "REG".into(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.write_only = true;
        ads.reason_set = true;
        ads.set_record_info("TEST:WFAIL", ScanType::Passive);

        let mut rec = WaveformRecord::new(64, DbFieldType::Char);
        rec.put_field("VAL", EpicsValue::CharArray(vec![0x41, 0x42]))
            .unwrap();
        ads.read(&mut rec).unwrap();

        // Generic asynError on an output record -> WRITE_ALARM/INVALID (NOT
        // READ_ALARM, which the path used before the direction fix).
        assert_eq!(
            ads.last_alarm(),
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
        fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
            self.writes.lock().unwrap().push(data.to_vec());
            self.sequence.lock().unwrap().push("write");
            Ok(())
        }
        fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
            self.sequence.lock().unwrap().push("read");
            let n = self.reply.len().min(buf.len());
            buf[..n].copy_from_slice(&self.reply[..n]);
            Ok(n)
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
            base: PortDriverBase::new(name, 1, PortFlags::default()),
            writes: writes.clone(),
            sequence: sequence.clone(),
            reply: reply.to_vec(),
        };
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let actor = PortActor::new(Box::new(port), rx);
        std::thread::Builder::new()
            .name("cmdresp-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        let handle = PortHandle::new(tx, name.into(), interrupts);
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
            timeout: Duration::from_secs(1),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        ads.octet_max_size = 128;

        // No command → plain read.
        match ads.read_op() {
            Some(RequestOp::OctetRead { buf_size }) => assert_eq!(buf_size, 128),
            other => panic!("expected OctetRead, got {other:?}"),
        }

        // Command cached → write-then-read carrying the command bytes.
        ads.octet_cmd = Some(b"*IDN?\r\n".to_vec());
        match ads.read_op() {
            Some(RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
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
            Arc::new(crate::trace::TraceManager::new()),
        );

        // The DRVINFO tail "*IDN?\r\n" is the literal command — the "\r\n" is two
        // escape sequences (four chars) in the link, decoded to 0x0D 0x0A.
        let ctx = DeviceSupportContext {
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
        crate::asyn_record::register_port(
            "cmdresp_nul",
            handle,
            Arc::new(crate::trace::TraceManager::new()),
        );

        // "AB\000CD": dbTranslateEscape yields A B 0x00 C D; C strlen stops at the
        // NUL, so only "AB" reaches the wire.
        let ctx = DeviceSupportContext {
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
        crate::asyn_record::register_port(
            "cmdresp_lnul",
            handle,
            Arc::new(crate::trace::TraceManager::new()),
        );

        // Raw DRVINFO "\000CD" is non-empty (C strlen != 0 -> no reject); it
        // escapes to [0x00,'C','D'] and truncates at the leading NUL -> empty
        // command -> C writes 0 bytes then reads.
        let ctx = DeviceSupportContext {
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
            dev.last_alarm(),
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
            timeout: Duration::from_secs(1),
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
            Some(EpicsValue::CharArray(b"PONG".to_vec())),
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
            timeout: Duration::from_secs(1),
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
            Some(EpicsValue::CharArray(b"STATUS=OK".to_vec())),
            "reply must populate the lsi VAL"
        );
    }

    /// An asynOctetCmdResponse record with an EMPTY command (DRVINFO escaped to
    /// nothing) is a misconfiguration: C initCmdBuffer (devAsynOctet.c:632-637)
    /// rejects it with LINK_ALARM/INVALID + INIT_ERROR. base-rs holds the record
    /// at LINK_ALARM/INVALID and performs NO I/O (no empty command is written).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cmd_response_empty_command_holds_link_alarm_and_writes_nothing() {
        use epics_base_rs::server::record::AlarmSeverity;
        use epics_base_rs::server::records::stringin::StringinRecord;

        let (handle, (writes, sequence)) = spawn_cmd_response_port("cmdresp_empty", b"unused");
        let link = AsynLink {
            port_name: "cmdresp_empty".into(),
            addr: 0,
            timeout: Duration::from_secs(1),
            drv_info: String::new(),
        };
        let mut ads = AsynDeviceSupport::from_handle(handle, link, "asynOctet");
        // What the factory builds for an empty DRVINFO: octet_cmd = Some(empty).
        ads.octet_cmd = Some(Vec::new());
        ads.reason_set = true;
        ads.set_record_info("TEST:EMPTY", ScanType::Passive);

        let mut rec = StringinRecord::new("");
        ads.read(&mut rec).unwrap();

        // No I/O at all — not even an empty write.
        assert!(
            writes.lock().unwrap().is_empty(),
            "empty command must not write to the device"
        );
        assert!(
            sequence.lock().unwrap().is_empty(),
            "empty command must perform no driver I/O"
        );
        // Held at LINK_ALARM / INVALID (C recGblSetSevr(LINK_ALARM, INVALID)).
        assert_eq!(
            ads.last_alarm(),
            Some((
                epics_base_rs::server::recgbl::alarm_status::LINK_ALARM,
                AlarmSeverity::Invalid as u16
            )),
            "empty command must hold the record at LINK_ALARM/INVALID"
        );
    }
}
