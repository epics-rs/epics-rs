use crate::error::{CaError, CaResult};
use crate::server::record::{FieldMetadataOverride, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::EpicsValue;

/// Histogram record — counts values into buckets.
///
/// C `histogramRecord.c`: bucket counters are `epicsUInt32`
/// (`cvt_dbaddr` sets `dbr_field_type = DBF_ULONG`) and wrap
/// explicitly at `UINT_MAX` (`add_count`: `if (*pdest == UINT_MAX)
/// *pdest = 0; (*pdest)++;`).
///
/// The counters are `epicsUInt32` here too — [`crate::types::DbFieldType::ULong`], served
/// as an [`EpicsValue::ULongArray`]. The declared type is the one C declares,
/// and each wire projects it the way C's does, so neither wire needs a
/// per-record special case:
///
/// * CA has no unsigned-long DBR type and promotes `DBF_ULONG` to
///   `DBR_DOUBLE` (`db_convert.h`: `6, /*DBR_ULONG to DBR_DOUBLE*/`) —
///   `cainfo HI` on softIoc reports `Native data type: DBF_DOUBLE`, the same
///   answer it gives for a `waveform` with `FTVL=ULONG`.
/// * PVA serves it natively: pvxs `ioc/typeutils.cpp:43-44` maps `DBR_ULONG`
///   to `TypeCode::UInt32`, so `VAL` is a `uint32[]`.
///
/// Counting wraps at `UINT_MAX` exactly as C does (`add_count`:
/// `if (*pdest == UINT_MAX) *pdest = 0; (*pdest)++;`) — `u32::wrapping_add`,
/// with no signed slot to reinterpret.
pub struct HistogramRecord {
    pub val: Vec<u32>, // Bucket counts (C epicsUInt32 — wraps at UINT_MAX)
    pub nelm: i32,     // Number of buckets
    pub ulim: f64,     // Upper limit
    pub llim: f64,     // Lower limit
    pub wdth: f64,     // Width of one bucket = (ulim-llim)/nelm
    pub sgnl: f64,     // Signal value to bin (C: DBF_DOUBLE, special(SPC_MOD))
    /// C `field(SVL,DBF_INLINK)` (`histogramRecord.dbd.pod:212`) — "Signal Value
    /// Location", the record's ONLY input link. `histogramRecord` declares NO
    /// INP: its soft device support reads SVL into SGNL
    /// (`devHistogramSoft.c:52` `dbGetLink(&prec->svl, DBR_DOUBLE, &prec->sgnl,
    /// 0, 0)` every process; `:44` `recGblInitConstantLink(&prec->svl,
    /// DBF_DOUBLE, &prec->sgnl)` at init), and `process()` bins whatever SGNL
    /// then holds. The port had no SVL at all and drove the record from a
    /// nonexistent INP, so `field(SVL,"MYSIG")` was dropped at load and the
    /// record was inert.
    pub svl: String,
    pub cmd: i16,   // 0=Read, 1=Clear, 2=Start, 3=Stop
    pub csta: bool, // Counting state — TRUE while counting is enabled
    pub sdel: f64,  // Signal deadband (monitor refresh period, seconds)
    pub mdel: i32,  // Monitor count deadband
    pub mcnt: i32,  // Counts accumulated since last monitor post
    // Simulation block (histogramRecord.dbd.pod:245-282). SIMM is
    // DBF_MENU menu(menuYesNo) (NO/YES), SIMS is DBF_MENU menu(menuAlarmSevr);
    // both served as DBR_ENUM shorts. SIOL/SIML are the simulation
    // input/mode links, SVAL the DBF_DOUBLE simulation value. SSCN (menuScan)
    // and OLDSIMM (menuSimm, SPC_NOMOD) are served by the common-field path
    // (`CommonFields::sscn` / `CommonFields::oldsimm`) — they are framework
    // state written only by the simulation-mode owner
    // (`RecordInstance::rec_gbl_save_simm` / `rec_gbl_check_simm`), never by
    // the record.
    pub simm: i16,
    pub siol: String,
    pub sval: f64,
    pub siml: String,
    pub sims: i16,
    pub sdly: f64,
    /// Did `init_record` pass 1 seed SGNL from a CONSTANT SVL? C
    /// `devHistogramSoft.c::init_record` (44-45): `if
    /// (recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl)) prec->udf =
    /// FALSE;`. UDF lives in the common fields, which `init_record` cannot
    /// reach, so the outcome is carried here and folded in by
    /// `post_init_finalize_undef` (the same shape aao uses for its constant DOL).
    constant_svl_loaded: bool,
    /// A `special(SPC_RESET)` write to SDEL asked for `wdogInit`
    /// (`histogramRecord.c:266-268`). `special()` has no database handle, so
    /// the request is latched here and drained by `take_special_actions` as a
    /// [`crate::server::record::ProcessAction::ArmWatchdog`] — the same shape
    /// the scaler uses for the `dbPutLink` its `special()` performs.
    rearm_watchdog: bool,
}

impl Default for HistogramRecord {
    fn default() -> Self {
        // C `histogramRecord.dbd.pod` `field(NELM,DBF_USHORT){ initial("1") }`:
        // an unset NELM defaults to 1 bucket, not 10. NELM is read-only and
        // sizes VAL, so a .db that omits it must read back NELM=1 / a
        // 1-element VAL to match C. ULIM and LLIM carry NO C `initial(...)`
        // (`field(ULIM,DBF_DOUBLE)` / `field(LLIM,DBF_DOUBLE)`), so C defaults
        // both to 0.0 and `init_record` derives WDTH = (ULIM-LLIM)/NELM = 0.
        let nelm = 1;
        let ulim = 0.0;
        let llim = 0.0;
        Self {
            val: vec![0; nelm as usize],
            nelm,
            ulim,
            llim,
            wdth: (ulim - llim) / nelm as f64,
            sgnl: 0.0,
            svl: String::new(),
            cmd: 0,
            // C init leaves CSTA at its DBD default; the histogram
            // record counts by default (CSTA defaults TRUE) so a
            // freshly created record accumulates without an explicit
            // CMD=2 start.
            csta: true,
            sdel: 0.0,
            mdel: 0,
            mcnt: 0,
            simm: 0,
            siol: String::new(),
            sval: 0.0,
            siml: String::new(),
            sims: 0,
            sdly: -1.0,
            constant_svl_loaded: false,
            rearm_watchdog: false,
        }
    }
}

impl HistogramRecord {
    pub fn new(nelm: i32, llim: f64, ulim: f64) -> Self {
        let n = nelm.max(1);
        Self {
            val: vec![0; n as usize],
            nelm,
            ulim,
            llim,
            wdth: (ulim - llim) / n as f64,
            ..Default::default()
        }
    }

    /// Recompute bucket width — C `special` SPC_RESET path
    /// (`prec->wdth = (ulim-llim)/nelm`).
    fn recompute_wdth(&mut self) {
        let n = self.nelm.max(1) as f64;
        self.wdth = (self.ulim - self.llim) / n;
    }

    /// Add one sample. Mirrors C `add_count` (histogramRecord.c:320-352):
    /// early-out when counting is stopped; reject out-of-range signal;
    /// pick the bucket with the closed-upper-edge `temp <= i*wdth`
    /// loop; wrap the `epicsUInt32` counter at `UINT_MAX`.
    pub fn add_count(&mut self) {
        if !self.csta {
            return;
        }
        // Inverted limits: count nothing. C's `add_count` also writes
        // stat/sevr=SOFT/INVALID directly here; the port raises that alarm
        // through `check_alarms` via the `nsta`/`nsev` owner instead, so it is
        // observable consistently on both paths (CBUG-F12 refused). The
        // counting-only path leaves the alarm out.
        if self.llim >= self.ulim || self.nelm <= 0 {
            return;
        }
        if self.sgnl < self.llim || self.sgnl >= self.ulim {
            return;
        }
        // C: temp = sgnl - llim; for (i=1; i<=nelm; i++) if (temp <= i*wdth) break;
        let temp = self.sgnl - self.llim;
        let mut i = 1i32;
        while i <= self.nelm {
            if temp <= i as f64 * self.wdth {
                break;
            }
            i += 1;
        }
        // C: pdest = bptr + i - 1. The loop can leave i == nelm+1 only
        // when rounding pushes temp past the last edge; clamp so the
        // index stays inside the buffer.
        let bucket = ((i - 1).max(0) as usize).min(self.val.len().saturating_sub(1));
        if bucket < self.val.len() {
            // C: if (*pdest == UINT_MAX) *pdest = 0; (*pdest)++;
            self.val[bucket] = self.val[bucket].wrapping_add(1);
            self.mcnt = self.mcnt.saturating_add(1);
        }
    }

    /// Back-compat helper: set SGNL then count one sample.
    pub fn add_sample(&mut self, value: f64) {
        self.sgnl = value;
        self.add_count();
    }

    /// C `clear_histogram` — zero every bucket and arm a monitor post.
    fn clear_histogram(&mut self) {
        for v in &mut self.val {
            *v = 0;
        }
        self.mcnt = self.mdel + 1;
    }
}

/// Choice labels for the histogram command menu, in index order.
/// C `menu(histogramCMD)` (`histogramRecord.dbd.pod`): 0=Read, 1=Clear,
/// 2=Start, 3=Stop. Writing `CMD` triggers the corresponding action on the
/// collection buffer.
const HISTOGRAM_CMD_CHOICES: &[&str] = &["Read", "Clear", "Start", "Stop"];

/// C `histogramRecord.c:88` `int histogramSDELprecision = 2;` — the precision
/// `get_precision` serves for `SDEL`, the monitor deadband.
const HISTOGRAM_SDEL_PRECISION: i16 = 2;

impl Record for HistogramRecord {
    fn record_type(&self) -> &'static str {
        "histogram"
    }

    /// `histogramRecord.c:458-475` `get_control_double` lists `VAL` (which the
    /// shared VAL-class route already serves) and `WDTH`, the bucket width,
    /// whose limits are the span the buckets cover: `ULIM - LLIM` up, `0.0`
    /// down. Without this `WDTH` falls to the `default:` arm and reports the
    /// DBF_DOUBLE range of ±1e300.
    /// `WDTH` is a COMPUTED span in both slots: `histogramRecord.c:467-469`
    /// (control) and `:450-452` (graphic) each serve `ULIM - LLIM` up and a
    /// literal `0.0` down — neither HOPR/LOPR nor the DBF_DOUBLE range, so it
    /// can take neither the VAL cache nor the routed `default:` arm.
    /// `SDEL`, the monitor-deadband seconds, is the other override: `get_units`
    /// (`:410-417`) answers it with a literal `"s"` and `get_precision`
    /// (`:419-437`) with `histogramSDELprecision` (`= 2`, `:88`). The precision
    /// switch's other cases (`ULIM`/`LLIM`/`SGNL`/`SVAL`/`WDTH`) serve
    /// `prec->prec`, which is the PREC arm the shared route already applies —
    /// `SDEL` is the one case that escapes it.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        if field.eq_ignore_ascii_case("WDTH") {
            return Some(FieldMetadataOverride {
                ctrl_limits: Some((self.ulim - self.llim, 0.0)),
                disp_limits: Some((self.ulim - self.llim, 0.0)),
                ..Default::default()
            });
        }
        field
            .eq_ignore_ascii_case("SDEL")
            .then(|| FieldMetadataOverride {
                units: Some("s".into()),
                precision: Some(HISTOGRAM_SDEL_PRECISION),
                ..Default::default()
            })
    }

    /// C `histogramRecord.c::process` NEVER clears UDF. The record's only two
    /// `prec->udf = FALSE` sites are `clear_histogram` (the CSTA/RESET special,
    /// `:361`) and the simulated SIOL read (`:387`) — so a histogram that has
    /// been counting for hours still reports UDF=1 (softIoc: `dbpf H1.PROC 1`
    /// → `H1.UDF` = 1, SEVR NO_ALARM, because histogram's `checkAlarms` has no
    /// UDF test either).
    fn clears_udf(&self) -> bool {
        false
    }

    /// `histogramRecord.c` has no UDF alarm test anywhere — its `process` goes
    /// straight from `readValue` to `monitor`, so the UDF=1 it carries raises
    /// nothing (softIoc: UDF=1, SEVR NO_ALARM, STAT NO_ALARM).
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// The invalid-limits alarm — C `add_count` (histogramRecord.c:329-334):
    ///
    /// ```c
    /// if (prec->llim >= prec->ulim) {
    ///     if (prec->nsev < INVALID_ALARM) {
    ///         prec->stat = SOFT_ALARM;   /* writes stat/sevr, NOT nsta/nsev */
    ///         prec->sevr = INVALID_ALARM;
    /// ```
    ///
    /// **DEVIATION from C, deliberate — CBUG-F12 refused.** An *inverted*-limits
    /// histogram (`LLIM > ULIM`) cannot bin anything: it is genuinely
    /// misconfigured, and the record's *intent* — the C code literally tries to
    /// set `SOFT_ALARM`/`INVALID_ALARM` — is to flag it. C's *mechanism* is
    /// broken: it writes `stat`/`sevr` DIRECTLY instead of `nsta`/`nsev` via
    /// `recGblSetSevr`, so the alarm's visibility depends on which trigger ran
    /// `add_count` — a path-dependent dual meaning:
    ///
    /// * process path (`process()` → `monitor()`): the cycle's
    ///   `recGblResetAlarms` copies `nsta/nsev`(0/0) over the direct write and
    ///   erases it before any client sees it — C shows NO_ALARM (dead code:
    ///   C's intent to alarm, C's behaviour NO_ALARM).
    /// * SGNL SPC_MOD `special()` path: no monitor follows `add_count`, so the
    ///   direct write STICKS and a `caget` reports STAT=SOFT SEVR=INVALID.
    ///
    /// Per `doc/strategy-2026-07-13.md` §2 — *C's bugs are not the contract;
    /// clean is the goal* — the port raises the alarm through the single
    /// `nsta`/`nsev` owner (`rec_gbl_set_sevr`), so a misconfigured histogram
    /// reports SOFT/INVALID CONSISTENTLY: committed by `recGblResetAlarms` on
    /// the process path, and committed by the SGNL special path's own
    /// post-`check_alarms` `recGblResetAlarms` (see `field_io.rs`). This refuses
    /// C's path-dependent dead-code/sticky behaviour and its direct-write
    /// anti-pattern in one move. `rec_gbl_set_sevr`'s raise-only rule subsumes
    /// C's `nsev < INVALID_ALARM` guard (it only raises when strictly higher).
    /// Gated on `csta` because C's `add_count` returns before the limits test
    /// when counting is stopped.
    ///
    /// # Why the alarm test is `>` where C's counting test is `>=`
    ///
    /// The deviation flags an *inversion* — a limit pair only an operator can
    /// create. It must not flag an *empty* range, because `LLIM == ULIM` is the
    /// state every histogram is in before anyone configures it: neither field
    /// carries an `initial(...)` in `histogramRecord.dbd`, so a freshly loaded
    /// record has `LLIM == ULIM == 0` and `CSTA == 1`. Testing `>=` here made
    /// the port raise SOFT/INVALID on the first process of *every* unconfigured
    /// histogram, where C shows NO_ALARM — measured by the differential oracle
    /// as 14 defects across `SCAN`/`PROC`/`UDF`, all of them a bare
    /// `record(histogram, "…") {}`. Refusing C's broken *mechanism* was never a
    /// licence to alarm on C's default state.
    ///
    /// [`Self::add_count`]'s guard stays `>=`, and is not the same test: an
    /// empty range bins nothing, which is arithmetic rather than a judgement
    /// about configuration, and C's `add_count` returns there too.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::record::AlarmSeverity;
        if self.csta && self.llim > self.ulim {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::SOFT_ALARM,
                AlarmSeverity::Invalid,
            );
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `process` → `add_count(prec)` then `monitor`. The signal
        // is read from the input link by the framework before
        // process(); count it into the current bucket. `monitor()`'s
        // count-deadband decision is `array_monitor_post` below.
        self.add_count();
        Ok(ProcessOutcome::complete())
    }

    /// C `histogramRecord.c::monitor` (:282-296) — the record's VAL-mask rule,
    /// and the ONLY writer of MCNT on the monitor path:
    ///
    /// ```c
    /// static void monitor(histogramRecord *prec) {
    ///     unsigned short monitor_mask = recGblResetAlarms(prec);
    ///     if (prec->mcnt > prec->mdel) {
    ///         monitor_mask |= DBE_VALUE | DBE_LOG;
    ///         prec->mcnt = 0;                    /* reset counts since monitor */
    ///     }
    ///     if (monitor_mask)
    ///         db_post_events(prec, &prec->val, monitor_mask);
    /// }
    /// ```
    ///
    /// MDEL is a COUNT deadband, not an analog one: VAL posts only once more
    /// than MDEL counts have landed since the last post. The port had no gate —
    /// an array VAL has no `to_f64`, so the generic deadband answered
    /// "always post" — which made MDEL inert (every process shipped the whole
    /// bin array to every subscriber, the exact traffic MDEL exists to prevent)
    /// and left MCNT a monotonically growing counter instead of "counts since
    /// the last posted VAL".
    ///
    /// Zeroing MCNT here, at the post, is what gives it that one meaning on
    /// every path — including the SDEL watchdog (`watchdog_fire`), which shares
    /// the counter and whose `mcnt > 0` test was reading a number that never
    /// went back down.
    ///
    /// `hash_changed: false` — histogram has no HASH field (that is the
    /// waveform/aai/aao MPST/APST mechanism); this hook is simply the owner of
    /// the VAL mask, and histogram's C `monitor()` fills it with its own rule.
    fn array_monitor_post(&mut self) -> Option<crate::server::record::ArrayMonitorPost> {
        let post = self.mcnt > self.mdel;
        if post {
            self.mcnt = 0;
        }
        Some(crate::server::record::ArrayMonitorPost {
            post_value: post,
            post_archive: post,
            hash_changed: false,
        })
    }

    /// C `histogramRecord.c::special` (:226-274), SPC_RESET arm:
    ///
    /// ```c
    /// case SPC_RESET:
    ///     if (dbGetFieldIndex(paddr) == histogramRecordSDEL) {
    ///         wdogInit(prec);                       /* re-arm the monitor watchdog */
    ///     } else {                                  /* ULIM / LLIM */
    ///         prec->wdth = (prec->ulim - prec->llim) / prec->nelm;
    ///         clear_histogram(prec);
    ///     }
    /// ```
    ///
    /// One owner for the record's three SPC_RESET fields
    /// (`histogramRecord.dbd.pod`: ULIM :183-189, LLIM :190-196, SDEL
    /// :239-244). The SDEL arm re-arms the watchdog through
    /// [`crate::server::record::ProcessAction::ArmWatchdog`] — `special()` has no database handle, so
    /// the framework performs the `wdogInit` on its behalf, drained
    /// immediately after this returns (`take_special_actions`).
    ///
    /// (SIMM's SPC_MOD pass is the framework's `recGblSaveSimm`/`CheckSimm`
    /// owner; SGNL's SPC_MOD `add_count` stays on the `put_field` arm, which is
    /// the only route C's `dbPutField` takes to it.)
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            "SDEL" => self.rearm_watchdog = true,
            "ULIM" | "LLIM" => {
                self.recompute_wdth();
                self.clear_histogram();
            }
            _ => {}
        }
        Ok(())
    }

    /// A `.SGNL` caput is C's SPC_MOD `special()` → `add_count`, which raises
    /// the inverted-limits alarm (histogramRecord.c:329-334) with no process
    /// cycle to follow. The put owner runs [`Record::check_alarms`] once the
    /// value is stored, then commits it via `recGblResetAlarms` (see
    /// `field_io.rs`), so the SOFT/INVALID the record raises is observable on
    /// this monitor-less path — CBUG-F12 refused (raised consistently, not
    /// path-dependent).
    fn special_checks_alarms(&self, put_field: &str) -> bool {
        put_field.eq_ignore_ascii_case("SGNL")
    }

    fn take_special_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        if std::mem::take(&mut self.rearm_watchdog) {
            vec![crate::server::record::ProcessAction::ArmWatchdog]
        } else {
            Vec::new()
        }
    }

    /// C `wdogInit` (:126-152): `if (prec->sdel > 0)` arm a delayed callback of
    /// SDEL seconds. A non-positive SDEL means no watchdog at all.
    ///
    /// SDEL is `DBF_DOUBLE` (histogramRecord.dbd.pod) and C stores the full
    /// double range verbatim — `caput REC.SDEL 1e308`/`inf` SUCCEEDS, arming a
    /// callback so far in the future it never fires. `Duration::from_secs_f64`
    /// PANICS on a non-finite or Duration-overflowing value, so it cannot be the
    /// converter here: a store of `1e308`/`1e39`/`inf` would abort the put the
    /// oracle expects to succeed. `try_from_secs_f64` maps every such value to
    /// `None` (no watchdog arms — behaviourally identical to C's never-firing
    /// callback), leaving the store itself accepted for the whole double range.
    fn watchdog_interval(&self) -> Option<std::time::Duration> {
        if self.sdel > 0.0 {
            std::time::Duration::try_from_secs_f64(self.sdel).ok()
        } else {
            None
        }
    }

    /// C `wdogCallback` (:102-124): when counts have accumulated since the last
    /// post (`mcnt > 0`), force a VAL post and zero MCNT. MDEL can hold every
    /// process-time post back indefinitely (`monitor()` posts only when
    /// `mcnt > mdel`, :283-291) — the watchdog is what makes a slow
    /// accumulation still reach a display, every SDEL seconds.
    ///
    /// The framework stamps the record (`recGblGetTimeStamp`) and posts
    /// `DBE_VALUE | DBE_LOG` for the returned field, then re-arms.
    fn watchdog_fire(&mut self) -> &'static [&'static str] {
        if self.mcnt > 0 {
            self.mcnt = 0;
            &["VAL"]
        } else {
            &[]
        }
    }

    /// `histogramRecord.dbd.pod` declares NO `INP` — the record's DBF_INLINK is
    /// `SVL` (:212), read into SGNL by `devHistogramSoft.c`. C's dbd rejects
    /// `field(INP,...)` on a histogram ("field not found"), and so does the
    /// common-field owner via this hook: a histogram must not be driveable from
    /// an INP the C record type does not have, or a `.db` written against the
    /// port is unloadable on a C IOC (and vice versa).
    fn declares_inp_link(&self) -> bool {
        false
    }

    /// C `devHistogramSoft.c::read_histogram` (50-54):
    /// `dbGetLink(&prec->svl, DBR_DOUBLE, &prec->sgnl, 0, 0)` on EVERY process.
    /// A CONSTANT (or unset) SVL is a no-op there — `dbConstGetValue` writes
    /// nothing — and was loaded once at init instead (see [`Self::init_record`]),
    /// so only a real link emits a fetch. Same gate as the aao's DOL.
    ///
    /// The value lands in SGNL through `put_field_internal`, NOT `put_field`:
    /// `dbGetLink` writes the field's memory directly and never runs C's
    /// `special()`, so the SPC_MOD `add_count` on a SGNL *caput* must not fire
    /// here — `process()` performs the cycle's single bin increment
    /// (`histogramRecord.c:218-219`).
    fn pre_input_link_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        if crate::server::recgbl::simm::is_constant(&crate::server::record::parse_link_v2(
            &self.svl,
        )) {
            return Vec::new();
        }
        vec![crate::server::record::ProcessAction::ReadDbLink {
            link_field: "SVL",
            target_field: "SGNL",
        }]
    }

    /// Internal (link / framework) delivery of a field value — C's
    /// direct-to-memory write, which does NOT run `special()`. SGNL is the one
    /// field where that distinction is observable on a histogram: its
    /// `put_field` arm carries the SPC_MOD `add_count` (`histogramRecord.c:334`),
    /// and routing a link read through it would count the sample twice (once
    /// here, once in `process()`).
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name.eq_ignore_ascii_case("SGNL") {
            self.sgnl = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch("SGNL".into()))?;
            return Ok(());
        }
        crate::server::record::put_field_internal_default(self, name, value)
    }

    /// C `devHistogramSoft.c::init_record` (40-48): a CONSTANT SVL seeds SGNL
    /// once — `recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl)` —
    /// and the record is then DEFINED. No bin increment: `add_count` runs only
    /// from `process()` and from the SGNL `special()`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `init_record` pass 0 (histogramRecord.c:149-161): size the bin
            // array, then `prec->wdth = (prec->ulim - prec->llim) / prec->nelm`.
            // This is WDTH's owner at load — the ULIM/LLIM `put_field` arms
            // only store, so a `.db` may declare NELM/ULIM/LLIM in any order.
            if self.nelm <= 0 {
                self.nelm = 1;
            }
            if self.val.len() != self.nelm as usize {
                self.val = vec![0; self.nelm as usize];
            }
            self.recompute_wdth();
            return Ok(());
        }
        if pass != 1 {
            return Ok(());
        }
        let svl = crate::server::record::parse_link_v2(&self.svl);
        if let Some(v) = crate::server::recgbl::simm::constant_load_value(&svl)
            && let Some(f) = v.to_f64()
        {
            self.sgnl = f;
            self.constant_svl_loaded = true;
        }
        Ok(())
    }

    /// The UDF half of the constant-SVL load (`devHistogramSoft.c:44-45`):
    /// `if (recGblInitConstantLink(...)) prec->udf = FALSE;`. UDF is a common
    /// field, which `init_record` cannot reach.
    fn post_init_finalize_undef(&mut self, udf: &mut bool) -> CaResult<()> {
        if self.constant_svl_loaded {
            *udf = false;
        }
        Ok(())
    }

    /// C `histogramRecord.c:384-387` — the simulated SIOL read lands in SGNL,
    /// not in VAL (VAL is the bin-count array):
    ///
    /// ```c
    /// status = dbGetLink(&prec->siol, DBR_DOUBLE, &prec->sval, 0, 0);
    /// if (status == 0) {
    ///     prec->sgnl = prec->sval;
    ///     prec->udf = FALSE;
    /// }
    /// ```
    ///
    /// and `process()` (`:218-219`) then bins it: `if (status == 0)
    /// add_count(prec);`. The framework calls this only on that same
    /// `status == 0`, so the bin increment belongs here — the assignment is a
    /// plain store, NOT the `put_field("SGNL")` path, whose `add_count` is C's
    /// SPC_MOD `special()` hook (`:260-263`) and would count the sample twice.
    fn land_simulated_value(&mut self, value: EpicsValue) -> CaResult<()> {
        self.sgnl = value.to_f64().unwrap_or(0.0);
        self.add_count();
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            // C's epicsUInt32 counters (DBF_ULONG), surfaced as-is.
            "VAL" => Some(EpicsValue::ULongArray(self.val.clone())),
            // The DBF types are the `.dbd.pod`'s: NELM is DBF_USHORT (:163),
            // MDEL/MCNT are DBF_SHORT (:229,:234). The value variant is what CA
            // and PVA project the native type from, so it must agree with the
            // `FieldDesc` — as VAL's ULongArray does.
            "NELM" => Some(EpicsValue::UShort(self.nelm as u16)),
            "ULIM" => Some(EpicsValue::Double(self.ulim)),
            "LLIM" => Some(EpicsValue::Double(self.llim)),
            "WDTH" => Some(EpicsValue::Double(self.wdth)),
            "SGNL" => Some(EpicsValue::Double(self.sgnl)),
            "SVL" => Some(EpicsValue::String(self.svl.clone().into())),
            "CMD" => Some(EpicsValue::Short(self.cmd)),
            "CSTA" => Some(EpicsValue::Short(if self.csta { 1 } else { 0 })),
            "SDEL" => Some(EpicsValue::Double(self.sdel)),
            "MDEL" => Some(EpicsValue::Short(self.mdel as i16)),
            "MCNT" => Some(EpicsValue::Short(self.mcnt as i16)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::Double(self.sval)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::ULongArray(arr) => {
                    self.val = arr;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            // ULIM/LLIM/SDEL are `special(SPC_RESET)` — the arms STORE ONLY.
            // The side effects belong to `special()` (C runs them in
            // `dbPutSpecial(paddr, 1)`, after `dbPut` stored the value), which
            // is also what keeps the load path C-faithful: dbStaticLib never
            // calls `special()`, and `init_record` pass 0 derives WDTH from the
            // fields as loaded, in either order.
            "ULIM" => match value {
                EpicsValue::Double(v) => {
                    self.ulim = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ULIM".into())),
            },
            "LLIM" => match value {
                EpicsValue::Double(v) => {
                    self.llim = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("LLIM".into())),
            },
            "SGNL" => {
                self.sgnl = value.to_f64().unwrap_or(0.0);
                // C `special` SPC_MOD on SGNL → add_count. This is the
                // `dbPutField` path ONLY — the link/device deliveries of SGNL
                // (SVL read, SIOL simulation read) write `prec->sgnl` directly
                // and never run `special()`, so they go through
                // `put_field_internal` / `land_simulated_value` instead and let
                // `process()` do the single bin increment.
                self.add_count();
                Ok(())
            }
            "SVL" => match value {
                EpicsValue::String(s) => {
                    self.svl = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVL".into())),
            },
            "CMD" => match value {
                EpicsValue::Short(v) => {
                    // Store the raw ordinal first — C `dbPut` writes `prec->cmd`
                    // before `special()` runs.
                    self.cmd = v;
                    // C `histogramRecord.c::special` SPC_CALC (:246-259) is an
                    // `if / else if` chain, NOT a catch-all:
                    //   cmd <= 1  → clear_histogram(); cmd = 0;
                    //   cmd == 2  → csta = TRUE;        cmd = 0;
                    //   cmd == 3  → csta = FALSE;       cmd = 0;
                    // An out-of-range ordinal (cmd >= 4) matches no branch, so C
                    // does NOTHING and the raw ordinal survives verbatim (no
                    // action, no reset-to-0). The `_ => clear_histogram(); cmd=0`
                    // the port had ran the Read/Clear arm for over-max ordinals
                    // and then zeroed cmd, which is exactly the divergence: a
                    // caput CMD '4' must read back CMD=4, not 0/"Read". (STAT=UDF
                    // SEVR=INVALID come from the fresh record's undefined state /
                    // the generic menu-put path, unchanged here.)
                    if v <= 1 {
                        self.clear_histogram();
                        self.cmd = 0;
                    } else if v == 2 {
                        self.csta = true;
                        self.cmd = 0;
                    } else if v == 3 {
                        self.csta = false;
                        self.cmd = 0;
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CMD".into())),
            },
            // `special(SPC_NOMOD)` — the load path only (see the FieldDesc).
            // At runtime CSTA moves through CMD's `special()`, never here.
            "CSTA" => match value {
                EpicsValue::Short(v) => {
                    self.csta = v != 0;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CSTA".into())),
            },
            "SDEL" => match value {
                EpicsValue::Double(v) => {
                    self.sdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDEL".into())),
            },
            "MDEL" => match crate::server::records::count_put(&value) {
                Some(v) => {
                    self.mdel = v;
                    Ok(())
                }
                None => Err(CaError::TypeMismatch("MDEL".into())),
            },
            "SIMM" => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMM".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => {
                    self.siol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SVAL" => match value {
                EpicsValue::Double(v) => {
                    self.sval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVAL".into())),
            },
            "SIML" => match value {
                EpicsValue::String(s) => {
                    self.siml = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIMS" => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMS".into())),
            },
            "SDLY" => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDLY".into())),
            },
            // C `field(NELM,DBF_USHORT){ promptgroup special(SPC_NOMOD) }`:
            // settable at `.db` load (dbStaticLib bypasses SPC_NOMOD), runtime-
            // immutable. The runtime block is the field_io `read_only` gate (the
            // FieldDesc carries `read_only: true`), so a CA/PVA caput is still
            // rejected; this arm serves only the load path (`apply_fields`),
            // sizing the bin array exactly like `new()`.
            "NELM" => match crate::server::records::count_put(&value) {
                Some(n) => {
                    let n = n.max(1);
                    self.nelm = n as i32;
                    self.val = vec![0; n as usize];
                    self.recompute_wdth();
                    Ok(())
                }
                None => Err(CaError::TypeMismatch("NELM".into())),
            },
            // WDTH/MCNT are computed runtime fields (no promptgroup) — never
            // client-writable, even at load. (OLDSIMM is the SPC_NOMOD saved
            // copy, refused by the common-field path that owns it.)
            "WDTH" | "MCNT" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    /// `CMD` is `DBF_MENU menu(histogramCMD)` (`histogramRecord.dbd.pod`);
    /// served as `DBR_ENUM` with the Read/Clear/Start/Stop choice labels.
    /// `SIMM` is `DBF_MENU menu(menuYesNo)` (`histogramRecord.dbd.pod:258-262`),
    /// served as `DBR_ENUM` with the NO/YES labels.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "CMD" => Some(HISTOGRAM_CMD_CHOICES),
            "SIMM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::dbd_generated;
    use crate::types::DbFieldType;

    // C `histogramRecord.dbd.pod` `field(NELM,DBF_USHORT){ initial("1") }` —
    // a histogram built without an explicit NELM defaults to 1 bucket (and a
    // 1-element VAL), not the old hand-coded 10.
    #[test]
    fn nelm_defaults_to_one_per_dbd_initial() {
        let rec = HistogramRecord::default();
        assert_eq!(rec.nelm, 1);
        assert_eq!(rec.val.len(), 1, "VAL is sized by NELM");
        // NELM is C's DBF_USHORT (histogramRecord.dbd.pod:163).
        assert_eq!(rec.get_field("NELM"), Some(EpicsValue::UShort(1)));
    }

    #[test]
    fn ulim_llim_default_to_zero_per_absent_dbd_initial() {
        // C `field(ULIM,DBF_DOUBLE)` / `field(LLIM,DBF_DOUBLE)` carry no
        // `initial(...)`, so both default to 0.0 and `init_record` derives
        // WDTH = (ULIM-LLIM)/NELM = 0.
        let rec = HistogramRecord::default();
        assert_eq!(rec.ulim, 0.0, "ULIM has no C initial -> 0.0");
        assert_eq!(rec.llim, 0.0, "LLIM has no C initial -> 0.0");
        assert_eq!(rec.wdth, 0.0, "WDTH = (ULIM-LLIM)/NELM = 0");
    }

    /// R17-84: the bins are C's `epicsUInt32` — `cvt_dbaddr`
    /// (`histogramRecord.c:299-308`) sets `field_type = dbr_field_type =
    /// DBF_ULONG`. Every wire then projects that ONE declared type its own way,
    /// and both projections already exist in the port:
    ///
    /// * CA has no unsigned-long DBR type, so `DBF_ULONG` promotes to
    ///   `DBR_DOUBLE`. softIoc (`cainfo`, EPICS 7 base):
    ///
    ///   ```text
    ///   HI  (histogram VAL)        Native data type: DBF_DOUBLE
    ///   WU  (waveform FTVL=ULONG)  Native data type: DBF_DOUBLE
    ///   WL  (waveform FTVL=LONG)   Native data type: DBF_LONG
    ///   ```
    ///
    /// * PVA serves it natively as `uint32[]` — pinned by
    ///   `epics-pva-rs::leaf_convert::tests::histogram_val_is_served_as_uint32_array`.
    ///
    /// The port declared VAL `DBF_LONG` and stored the counts in an `i32` slot
    /// "as a bit container", which made CA answer DBR_LONG and PVA `int32[]`.
    #[test]
    fn val_is_dbf_ulong_and_promotes_to_dbr_double_on_ca() {
        let rec = HistogramRecord::new(2, 0.0, 10.0);

        let val_desc = dbd_generated::HISTOGRAM_FIELDS
            .iter()
            .find(|f| f.name == "VAL")
            .expect("histogram declares VAL");
        assert_eq!(
            val_desc.dbf_type,
            DbFieldType::ULong,
            "C cvt_dbaddr: dbr_field_type = DBF_ULONG"
        );

        let val = rec.get_field("VAL").expect("histogram serves VAL");
        assert!(
            matches!(val, EpicsValue::ULongArray(_)),
            "the bins are epicsUInt32, got {val:?}"
        );
        assert_eq!(
            val.dbr_type(),
            DbFieldType::Double,
            "CA promotes DBF_ULONG to DBR_DOUBLE (cainfo HI: DBF_DOUBLE)"
        );
    }

    /// C `add_count`: `if (*pdest == UINT_MAX) *pdest = 0; (*pdest)++;` — a
    /// counter at `UINT_MAX` wraps to 0 and never panics on overflow.
    #[test]
    fn counter_wraps_at_u32_max_no_panic() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.val[0] = u32::MAX;
        rec.sgnl = 1.0;
        rec.add_count();
        assert_eq!(rec.val[0], 0, "epicsUInt32 counter wraps UINT_MAX -> 0");
    }

    /// CMD=3 stops counting, CMD=2 resumes it.
    #[test]
    fn cmd_start_stop_pauses_counting() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.put_field("CMD", EpicsValue::Short(3)).unwrap(); // stop
        assert!(!rec.csta);
        rec.add_sample(1.0);
        assert_eq!(rec.val[0], 0, "stopped histogram must not count");
        rec.put_field("CMD", EpicsValue::Short(2)).unwrap(); // start
        assert!(rec.csta);
        rec.add_sample(1.0);
        assert_eq!(rec.val[0], 1, "started histogram counts again");
    }

    /// SIMM is `menu(menuYesNo)` served as DBR_ENUM with the NO/YES labels.
    #[test]
    fn simm_snapshot_is_enum_with_yesno_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(1)));
        let inst = RecordInstance::new("HIST:SIMM".into(), rec);
        let snap = inst.snapshot_for_field("SIMM").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
    }

    /// CMD<=1 clears all buckets.
    #[test]
    fn cmd_clear_zeros_buckets() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.add_sample(1.0);
        rec.add_sample(6.0);
        rec.put_field("CMD", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.val, vec![0, 0]);
    }

    /// closed-upper-edge bucket selection — a value exactly on an
    /// internal boundary lands in the lower bucket.
    #[test]
    fn bin_boundary_uses_closed_upper_edge() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0); // wdth = 5
        rec.add_sample(5.0); // temp=5 <= 1*5 → bucket 0
        assert_eq!(rec.val, vec![1, 0]);
        rec.add_sample(5.0001); // temp>5 → bucket 1
        assert_eq!(rec.val, vec![1, 1]);
    }

    /// Out-of-range signal is rejected (C `add_count` early return).
    #[test]
    fn out_of_range_signal_rejected() {
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);
        rec.add_sample(-1.0);
        rec.add_sample(10.0); // >= ulim → rejected
        rec.add_sample(99.0);
        assert_eq!(rec.val, vec![0, 0]);
    }

    /// Simulation block (histogramRecord.dbd.pod:245-282) is served:
    /// SIML/SIOL links, SVAL (DBF_DOUBLE), SIMS (menuAlarmSevr) round-trip;
    /// OLDSIMM (menuSimm, SPC_NOMOD) is readable but not client-writable;
    /// SIMS/OLDSIMM labels come from the shared menu registry.
    #[test]
    fn sim_block_served_and_oldsimm_read_only() {
        use crate::server::record::RecordInstance;
        let mut rec = HistogramRecord::new(2, 0.0, 10.0);

        rec.put_field("SIML", EpicsValue::String("sim:mode".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIML"),
            Some(EpicsValue::String("sim:mode".into()))
        );
        rec.put_field("SIOL", EpicsValue::String("sim:in".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIOL"),
            Some(EpicsValue::String("sim:in".into()))
        );
        rec.put_field("SVAL", EpicsValue::Double(3.5)).unwrap();
        assert_eq!(rec.get_field("SVAL"), Some(EpicsValue::Double(3.5)));
        rec.put_field("SIMS", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SIMS"), Some(EpicsValue::Short(2)));

        // OLDSIMM is special(SPC_NOMOD), and lives in the common fields (the
        // simulation-mode owner's latch) — readable, not client-writable.
        let mut inst = RecordInstance::new("HIST:SIM".into(), rec);
        assert!(matches!(
            inst.put_common_field("OLDSIMM", EpicsValue::Short(1)),
            Err(CaError::ReadOnlyField(_))
        ));
        assert_eq!(inst.get_common_field("OLDSIMM"), Some(EpicsValue::Short(0)));

        // SIMS (menuAlarmSevr) and OLDSIMM (menuSimm) resolve centrally.
        let sims = inst.snapshot_for_field("SIMS").unwrap();
        assert_eq!(
            sims.enums.as_ref().unwrap().strings,
            vec!["NO_ALARM", "MINOR", "MAJOR", "INVALID"]
        );
        let old = inst.snapshot_for_field("OLDSIMM").unwrap();
        assert_eq!(
            old.enums.as_ref().unwrap().strings,
            vec!["NO", "YES", "RAW"]
        );
    }
}
