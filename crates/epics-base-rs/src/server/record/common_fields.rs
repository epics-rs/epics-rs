use crate::types::WallTime;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use super::alarm::{AlarmSeverity, AnalogAlarmConfig};
use super::scan::{ScanType, SimModeScan};
use super::sparse_text::SparseText;
use crate::types::PvString;

/// `BKPT` (`DBF_UCHAR` in `dbCommon.dbd`) — the breakpoint debugger's
/// `BKPT_ON_MASK` / `BKPT_PRINT_MASK` byte.
///
/// C reads and writes `precord->bkpt` with no lock held: `dbstat`
/// (`dbBkpt.c:930`), `dbap` (`:860-866`) and `dbb`'s first test (`:296`)
/// all run outside `dbScanLock`. The byte is therefore atomic, and the
/// record cell keeps a handle to the same byte so those commands can reach
/// it without the record's lock set — which a continuation thread holds
/// for the whole of the chain it is stepping.
///
/// `Clone` makes a fresh byte with the same value, as cloning a record's
/// fields makes a different record; `Self::share` is the cell's handle to
/// this one.
#[derive(Debug, Default)]
pub struct BkptFlag(Arc<AtomicU8>);

impl BkptFlag {
    pub fn get(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, value: u8) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub(crate) fn share(&self) -> BkptFlag {
        BkptFlag(self.0.clone())
    }
}

impl Clone for BkptFlag {
    fn clone(&self) -> Self {
        BkptFlag(Arc::new(AtomicU8::new(self.get())))
    }
}

/// See [`CommonFields::time_last`]. Outside the crate it can only be read or
/// reset to the default, so `begin_stamp` stays its one writer.
#[derive(Clone, Copy, Debug)]
pub struct TimeLast(WallTime);

impl TimeLast {
    pub fn get(self) -> WallTime {
        self.0
    }
}

impl Default for TimeLast {
    fn default() -> Self {
        TimeLast(crate::runtime::general_time::epics_epoch())
    }
}

/// Proof that [`CommonFields::begin_stamp`] ran for this cycle.
#[must_use]
pub(crate) struct StampBegun(());

/// [`CommonFields`]' strings. A link field's text is served and re-parsed
/// on a put; the cycle reads the parse cached on the instance, never these.
#[derive(Clone, Debug, Default)]
pub struct CommonText {
    pub flnk: String,
    pub inp: String,
    pub out: String,
    pub tsel: String,
    /// Access security group — C `dbCommon.ASG`, a `DBF_STRING` with NO
    /// `initial()` in `dbCommon.dbd`, so a record that does not name a group
    /// holds the EMPTY string and `caget -t REC.ASG` on any C IOC prints an
    /// empty line. It is `asAddMember` (asLibRoutines.c:893-928) that resolves
    /// an empty or unknown name to the DEFAULT group — the FIELD never says
    /// "DEFAULT" unless the `.db` put it there. Ask [`CommonFields::access_group`] for
    /// the group to evaluate against; read this only to serve the field.
    pub asg: String,
    /// Description — C `dbCommon` `field(DESC,DBF_STRING) size(41)`. A
    /// genuine DBF_STRING data field served verbatim to clients, so it is
    /// a byte-preserving [`PvString`] (a non-UTF-8 DESC put must round-trip
    /// unchanged, matching EPICS fixed-size char-array string semantics).
    pub desc: PvString,
    /// Event name for `SCAN="Event"` records. C `dbCommon.dbd.pod`:
    /// `field(EVNT,DBF_STRING) { size(40) }` — since EPICS 7 this is
    /// an event *name* (resolved by `eventNameToHandle`), not a
    /// numeric subscript. A numeric string ("5") still works for
    /// backward compatibility. Empty means "no event".
    pub evnt: String,
    pub sdis: String,
}

/// Common fields shared by all records.
///
/// `repr(C)`, fields no process cycle reads first, so that the ones an
/// unmonitored cycle does read (from `disv` on, in the order it reads them)
/// sit on the fewest cache lines at the tail, next to [`RecordInstance`]'s
/// own hot fields.
///
/// [`RecordInstance`]: super::RecordInstance
#[derive(Clone, Debug)]
#[repr(C)]
pub struct CommonFields {
    /// The strings no process cycle reads, off the record's cycle lines.
    pub text: Box<CommonText>,
    pub bkpt: BkptFlag,
    /// Time-tag — C `dbCommon.dbd.pod` `field(UTAG,DBF_UINT64)`. A
    /// 64-bit user/hardware tag set alongside `time` by
    /// `recGblGetTimeStampSimm` via `dbGetTimeStampTag`. Zero when no
    /// time-tag source is configured.
    pub utag: u64,
    // Alarm hysteresis (analog records)
    pub hyst: f64,
    // Scan
    pub scan: ScanType,
    // SSCN's dbd default is the out-of-range sentinel 65535 ("use SCAN"),
    // unrepresentable in `ScanType`; see [`SimModeScan`].
    pub sscn: SimModeScan,
    // Alarm acknowledgement
    pub acks: AlarmSeverity,
    /// `UDFS` (`DBF_MENU menu(menuAlarmSevr)`) — the severity raised for a UDF
    /// record. Holds the **raw stored ordinal** (see [`AnalogAlarmConfig`]): C
    /// stores `(epicsEnum16)` verbatim on a numeric put, so an out-of-range
    /// `UDFS` round-trips (`caput REC.UDFS 4` → `4`); the alarm meaning is
    /// `AlarmSeverity::from_u16(udfs as u16)`.
    pub udfs: i16,
    /// `OLDSIMM` (`DBF_MENU menu(menuSimm)`, `special(SPC_NOMOD)`) — the
    /// PREVIOUS simulation mode, the latch C's `recGblSaveSimm`
    /// (`recGbl.c:421-425`) writes and `recGblCheckSimm` (`recGbl.c:427-437`)
    /// compares against to detect a SIMM transition and swap SCAN with SSCN.
    ///
    /// C declares it per record (in each of the 21 dbd files that carry SSCN);
    /// this port keeps it next to `sscn` in the common fields, for the same
    /// reason `sscn` lives here — it is framework state, written by exactly one
    /// owner (`PvDatabase::rec_gbl_save_simm`) and read by exactly one
    /// (`rec_gbl_check_simm`). Read-only to clients (SPC_NOMOD).
    pub oldsimm: i16,
    /// `PINI` is `DBF_MENU`/`menu(menuPini)` — a six-choice lifecycle
    /// selector, not a flag (see [`crate::server::record::PiniMode`] for the choice semantics). Holds
    /// the **raw stored ordinal** as `i16`: C stores `(epicsEnum16)` verbatim on
    /// a numeric put, so `caput REC.PINI 6` keeps `6` and `caput REC.PINI -1`
    /// keeps `65535`. `doRecordPini` compares against the exact valid indices,
    /// so any out-of-range ordinal simply matches no lifecycle pass — read a
    /// choice with `PiniMode::from_u16(pini as u16)`.
    pub pini: i16,
    // Phase/priority/event
    pub phas: i16,
    pub prio: i16,
    pub ackt: bool,
    /// Access security level. C `dbCommon.ASL`
    /// (0 or 1, default 0). Compared against `RULE(N, …)` levels
    /// in [`crate::server::access_security::AccessSecurityConfig::check_access_method`] —
    /// a rule with `RULE(M, …)` only applies when `ASL ≤ M`. The
    /// earlier code hard-coded ASL=0 at every ACF call site
    /// (CA tcp.rs, PVA native_source GET/PUT/MONITOR), so every
    /// `RULE(N>0, WRITE)` was always considered "applicable" and
    /// the per-record ASL gate was silently inert.
    pub asl: u8,
    // DISP — disable putfield from CA. `DBF_UCHAR` in `dbCommon.dbd`: C
    // stores the raw put byte and serves it SIGNED as `DBR_CHAR`
    // (`caput DISP 255` → `caget` = -1). Raw `u8` like [`Self::tpro`];
    // consumers test `!= 0`.
    pub disp: u8,
    // PROC — force-processing field. `DBF_UCHAR` in `dbCommon.dbd`
    // (`field(PROC,DBF_UCHAR){ pp(TRUE) }`): C's `dbPut` stores the raw put
    // byte in `prec->proc` and never resets it (retained across processing),
    // serving it back SIGNED as `DBR_CHAR` (`caput PROC 255` → `caget` = -1).
    // Raw `u8` like [`Self::disp`]/[`Self::rpro`]; the `pp(TRUE)` reprocess is
    // orthogonal and driven by the put-path force-process intercept.
    pub proc_field: u8,
    // Read every process cycle from here on.
    // Disable support
    pub disv: i16,
    pub disa: i16,
    /// `DISS` (`DBF_MENU menu(menuAlarmSevr)`) — the severity a disabled record
    /// takes. Holds the **raw stored ordinal** (see [`AnalogAlarmConfig`]).
    pub diss: i16,
    // Lock count (re-entrance counter)
    pub lcnt: i16,
    // New alarm state (pending, transferred by rec_gbl_reset_alarms)
    pub nsev: AlarmSeverity,
    pub nsta: u16,
    // Alarm state (current/result)
    pub sevr: AlarmSeverity,
    pub stat: u16,
    pub tse: i16,
    // Process control
    pub putf: bool,
    /// `TPRO` (`DBF_UCHAR` in `dbCommon.dbd`) — trace-processing flag. C
    /// stores the raw put byte and serves it as SIGNED `DBR_CHAR`
    /// (`caput TPRO 255` → `caget` = -1). Modeled as the raw `u8`, like
    /// [`Self::disp`], so the byte round-trips; consumers test `!= 0`.
    pub tpro: u8,
    // RPRO — reprocess flag. `DBF_UCHAR`, raw-byte readback like DISP/TPRO.
    pub rpro: u8,
    /// `UDF` (`DBF_UCHAR` in `dbCommon.dbd`) — "value undefined". C models it
    /// as one `epicsUInt8` that is BOTH the predicate (`if (prec->udf)`) and
    /// the served byte: `caput UDF 255` stores 255 and `caget` renders it
    /// signed `DBR_CHAR` (-1). Modeled as the raw `u8` so the byte round-trips
    /// on records that do not re-derive it; every predicate reader tests
    /// `!= 0`, and the record-facing [`crate::server::record::ProcessContext::udf`] boundary keeps the
    /// `bool` view.
    pub udf: u8,
    // Device
    /// `DTYP` and the soft-channel class it names, taken together — see
    /// [`Dtyp`](crate::server::device_support::Dtyp).
    pub dtyp: crate::server::device_support::Dtyp,
    // Timestamp
    pub time: WallTime,
    /// C `process()`'s local `timeLast`: [`Self::time`] as it stood when this
    /// cycle's stamp began, before any device-supplied time. The AFTC filter
    /// measures its gap from here.
    pub time_last: TimeLast,
    // Analog alarm config (Some for analog record types)
    pub analog_alarm: Option<AnalogAlarmConfig>,
    /// Pending alarm message — set during process(), transferred to
    /// `amsg` by `rec_gbl_reset_alarms` (epics-base PR #566).
    pub namsg: SparseText,
    /// Alarm message string (current). Mirrors epics-base PR #568:
    /// records may attach a human-readable explanation alongside
    /// `stat`/`sevr`. Empty means "no message". Transferred from
    /// `namsg` by `rec_gbl_reset_alarms`.
    pub amsg: SparseText,
    /// MLST/ALST for a record type that carries no cell of its own: the value
    /// its last `DBE_VALUE` / `DBE_LOG` post carried, 0 until one does.
    pub mlst: f64,
    pub alst: f64,
}

impl CommonFields {
    /// **The single owner of "which access group does this record belong to"** —
    /// C `asAddMember(&prec->asp, prec->asg)`, whose `asAddMemberPvt`
    /// (asLibRoutines.c:893-928) resolves an empty or unknown group name to the
    /// always-present `DEFAULT` group.
    ///
    /// Every access-security call site asks this, never [`CommonText::asg`] directly:
    /// the FIELD is what the `.db` wrote (empty by default, and that is what the
    /// wire must show), the GROUP is what the ACF is evaluated against. Three
    /// call sites used to spell the empty→DEFAULT rule out for themselves and a
    /// fourth (the CA server) relied on the config lookup missing — which is a
    /// different rule (C's unknown-NAME reassignment) that happened to have the
    /// same effect.
    pub fn access_group(&self) -> &str {
        if self.text.asg.is_empty() {
            "DEFAULT"
        } else {
            &self.text.asg
        }
    }

    /// Build a [`ProcessContext`](super::record_trait::ProcessContext)
    /// snapshot of the framework-owned state a record's `process()` or
    /// device support's `read()` needs to observe during the cycle.
    pub fn process_context(&self) -> super::record_trait::ProcessContext {
        super::record_trait::ProcessContext {
            udf: self.udf,
            udfs: AlarmSeverity::from_u16(self.udfs as u16),
            nsev: self.nsev,
            phas: self.phas,
            tse: self.tse,
            time: self.time,
            soft: self.dtyp.soft(),
            callback_priority: self.callback_priority(),
        }
    }

    /// Open this cycle's stamp: the only writer of [`Self::time_last`], and
    /// the only source of the token `TselStamp::stamp_begun` takes.
    #[inline(always)]
    pub(crate) fn begin_stamp(&mut self) -> StampBegun {
        self.time_last = TimeLast(self.time);
        StampBegun(())
    }

    /// The callback band this record's `PRIO` selects — C
    /// `callbackSetPriority(prec->prio, ...)` (`seqRecord.c:146`).
    pub fn callback_priority(&self) -> crate::runtime::task::CallbackPriority {
        crate::runtime::task::CallbackPriority::from_record_prio(self.prio)
    }
}

impl Default for CommonFields {
    fn default() -> Self {
        Self {
            sevr: AlarmSeverity::NoAlarm,
            // C dbd `field(STAT,DBF_MENU){ menu(menuAlarmStat) initial("UDF") }`
            // (`dbCommon.dbd.pod:296-301`): a record is born UNDEFINED, not
            // NO_ALARM. SEVR has no `initial()` — it starts NO_ALARM and is
            // raised to UDFS by the init prologue
            // (`RecordInstance::run_init_passes`, C `iocInit.c:521-523`), which
            // keys off exactly this STAT value.
            stat: crate::server::recgbl::alarm_status::UDF_ALARM,
            amsg: SparseText::default(),
            nsev: AlarmSeverity::NoAlarm,
            nsta: 0,
            namsg: SparseText::default(),
            acks: AlarmSeverity::NoAlarm,
            ackt: true,
            udf: 1,
            udfs: AlarmSeverity::Invalid as i16,
            scan: ScanType::Passive,
            // C dbd `field(SSCN,DBF_MENU){ menu(menuScan) initial("65535") }`:
            // the default is the out-of-range "use SCAN" sentinel, not Passive.
            sscn: SimModeScan::default(),
            // C dbd `field(OLDSIMM,DBF_MENU){ menu(menuSimm) }` — no
            // `initial()`, so it starts at index 0 (`menuSimmNO`).
            oldsimm: 0,
            pini: 0,
            tpro: 0,
            bkpt: BkptFlag::default(),
            text: Box::default(),
            dtyp: crate::server::device_support::Dtyp::default(),
            // An `epicsTimeStamp {0,0}` — C's never-processed `dbCommon.time`
            // — is the EPICS epoch, not the Unix epoch. Seeding this with
            // `UNIX_EPOCH` made a never-processed record publish
            // `timeStamp.secondsPastEpoch = 0` on PVA where pvxs publishes
            // 631152000 (`iocsource.cpp:240` adds POSIX_TIME_AT_EPICS_EPOCH
            // to the record's raw EPICS seconds). See
            // [`crate::runtime::general_time::epics_epoch`].
            time: crate::runtime::general_time::epics_epoch(),
            time_last: TimeLast::default(),
            tse: 0,
            utag: 0,
            analog_alarm: None,
            asl: 0,
            phas: 0,
            prio: 0,
            disv: 1,
            disa: 0,
            diss: 0,
            hyst: 0.0,
            lcnt: 0,
            disp: 0,
            putf: false,
            rpro: 0,
            proc_field: 0,
            mlst: 0.0,
            alst: 0.0,
        }
    }
}
