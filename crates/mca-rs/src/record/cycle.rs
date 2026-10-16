//! The acquisition cycle — C `mcaRecord.c::process` (`:491-845`).
//!
//! C runs the whole cycle inside `process()`, calling `pdset->send_msg()` from
//! the middle of it: setup commands, then the status read, then the data read,
//! then the ROI sums. The ORDER is load-bearing. C says so itself at `:636`
//! ("Turn acquisition on or off. Do this before reading device status") and
//! again at `:640-644`: `ACQG` is FORCED to 1 when the start command goes out,
//! because a device that finishes before the first status read would otherwise
//! never show a 1 -> 0 transition, and the record would never read the spectrum.
//!
//! A port record cannot call its device support from `process()` — the
//! framework calls `DeviceSupport::read()` BEFORE `Record::process()`. So the
//! half of C's `process` that crosses into the driver, C `:509-790`, is
//! [`McaRecord::begin_device_cycle`], which device support's `read_begin`
//! invokes on every pass with its [`McaDset`], and
//! [`McaRecord::finish_device_cycle`], which its `read()` invokes: the record
//! sends each message at C's site and decides what each answer, and each
//! failure, means. A read whose callback re-enters the record splits the pass
//! where C's `send_msg` returns. C `:793-841` (ROI sums, preset
//! stop, commit ACQG, UDF, STIM, alarms, forward link) is the record's
//! `process()`.
//!
//! The record therefore remains the only writer of its own state, and device
//! support remains the only thing that touches hardware. What moved is where the
//! device I/O is INVOKED from, not who decides what it means.

use std::time::SystemTime;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::recgbl::get_time_stamp;
use epics_base_rs::server::record::{CommonFields, ProcessContext};
use epics_base_rs::types::{EpicsValue, PvString, WallTime, c_cast};

use super::McaRecord;

/// A message from the record to device support — C's `mcaCommand` enum
/// (`mca.h:14-36`). It is the record/device-support interface, so it is
/// reproduced exactly and in C's order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum McaCommand {
    /// `mcaData` — read the spectrum.
    Data,
    StartAcquire,
    StopAcquire,
    Erase,
    ReadStatus,
    /// `mcaChannelAdvanceSource` — carries the `CHAS` menu index.
    ChannelAdvanceSource(i32),
    NumChannels(i32),
    /// `mcaAcquireMode` — carries the `MODE` menu index (PHA / MCS / List).
    /// C sends it only from `init_record` (`mcaRecord.c:486-487`); a MODE put
    /// goes out as [`McaCommand::for_mode`].
    AcquireMode(i32),
    Sequence(i32),
    Prescale(i32),
    PresetSweeps(i32),
    PresetLowChannel(i32),
    PresetHighChannel(i32),
    DwellTime(f64),
    PresetLiveTime(f64),
    PresetRealTime(f64),
    PresetCounts(f64),
}

impl McaCommand {
    /// What a MODE put sends: C passes the MODE index itself as the command
    /// code, `send_msg(pmca, pmca->mode, NULL)` (`mcaRecord.c:630`), so PHA,
    /// MCS and List arrive as `mcaData`, `mcaStartAcquire` and
    /// `mcaStopAcquire`. `None` for an index outside `menu(mcaMODE)`.
    pub fn for_mode(mode: u16) -> Option<Self> {
        match mode {
            0 => Some(Self::Data),
            1 => Some(Self::StartAcquire),
            2 => Some(Self::StopAcquire),
            _ => None,
        }
    }
}

/// C's `-1` from `send_msg` or `read_array`: device support did not
/// acknowledge the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nack;

/// The two messages device support may answer with a callback
/// (`mcaRecord.c:318-321`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McaRead {
    /// `mcaReadStatus`, which C's device answers by setting `rdns`.
    Status,
    /// `mcaData`, which C's device answers by setting `rdng`.
    Data,
}

/// How an [`McaRead`] went out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// It completed inside the send; C's device left `rdns`/`rdng` clear.
    Done,
    /// It is queued, and its callback re-enters the record on the port's
    /// thread; the pass ends there (C's `return(0)`, `:681`, `:778`).
    Queued,
    /// Its callback already ran on this thread — C `queueRequest` on a port
    /// without `ASYN_CANBLOCK` — and the record pass it re-enters runs nested
    /// before this one goes on.
    Ran,
}

/// The `mcaDSET` entries the record calls (`mcaRecord.c:158-168`), each
/// answering C's `(-1,0)=>(failure,success)` as `Err(Nack)` / `Ok`.
pub trait McaDset {
    /// `send_msg(pmca, command, parg)`.
    fn send_msg(&mut self, command: McaCommand) -> Result<(), Nack>;
    /// `send_msg(pmca, mcaReadStatus | mcaData, ...)` on the pass that
    /// starts the read. A device that completes it inside the send keeps
    /// the default: the status is then whatever [`Self::read_status`]
    /// returns, and the data is sent as a plain [`Self::send_msg`].
    fn send_read(&mut self, read: McaRead) -> Result<Sent, Nack> {
        match read {
            McaRead::Status => Ok(Sent::Done),
            McaRead::Data => self.send_msg(McaCommand::Data).map(|()| Sent::Done),
        }
    }
    /// `send_msg(pmca, mcaReadStatus, pmca->pstatus)`: the status read back.
    fn read_status(&mut self, mca: &McaRecord) -> Result<McaStatus, Nack>;
    /// `read_array(pmca)`: land the spectrum through
    /// [`McaRecord::land_spectrum_read`] or [`McaRecord::land_channel_count`].
    fn read_array(&mut self, mca: &mut McaRecord) -> Result<(), Nack>;
}

/// What device support reads back from the hardware — C `mcaStatus`
/// (`mca.h:41-48`), the struct `send_msg(mcaReadStatus, pmca->pstatus)` fills.
///
/// In C this hangs off the record's `PSTATUS` field, which is `DBF_NOACCESS` (a
/// bare `void *`): no CA representation, no row in the generated field table. It
/// is record state, not a field, and is held as such here.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct McaStatus {
    pub acquiring: bool,
    pub elapsed_real: f64,
    pub elapsed_live: f64,
    pub total_counts: f64,
    pub dwell_time: f64,
    /// C's `mcaStatus` carries a `deadTime` member that no code anywhere writes
    /// or reads — the record computes `DTIM`/`IDTIM` from the elapsed times
    /// instead. Kept so this struct IS `mcaStatus` rather than a lookalike.
    pub dead_time: f64,
}

/// Where a pass's device half stopped, for the `read()` that finishes it.
///
/// One per pass, innermost on top: a nested pass runs between its outer
/// pass's begin and finish, as C's nested `process` calls do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Resume {
    /// The pass reached the tail: nothing is left for `read()`.
    #[default]
    Nothing,
    /// Its status read ran inline: C's code after that `send_msg` returns.
    AfterStatus,
    /// Its data read ran inline: C's code after that `send_msg` returns.
    AfterData,
}

/// C's nesting is at most three `process` calls deep (one per read trip).
const RESUME_DEPTH: usize = 3;

/// The open passes' resume points, for [`McaRecord::finish_device_cycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResumeStack {
    at: [Resume; RESUME_DEPTH],
    len: usize,
}

impl ResumeStack {
    fn push(&mut self, resume: Resume) {
        assert!(self.len < RESUME_DEPTH, "mca passes nested past C's three");
        self.at[self.len] = resume;
        self.len += 1;
    }

    fn pop(&mut self) -> Resume {
        let Some(len) = self.len.checked_sub(1) else {
            return Resume::Nothing;
        };
        self.len = len;
        std::mem::take(&mut self.at[len])
    }
}

/// What [`McaRecord::begin_device_cycle`] left for the pass: the answers of
/// [`epics_base_rs::server::device_support::ReadStart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McaStart {
    /// Run `read()` now; it calls [`McaRecord::finish_device_cycle`].
    Synchronous,
    /// A read is queued and the pass ends with the record PACT.
    Pending,
    /// A read's callback ran inline; the pass it re-entered runs nested,
    /// then this pass's `read()` finishes it.
    Reentered,
}

impl McaRecord {
    /// C `process` `:509-790` up to the first read that is still out: every
    /// message the cycle sends device support, at C's site and in C's
    /// order, and what each answer means. Device support's `read_begin`
    /// calls it on every pass, the re-entered ones included.
    ///
    /// `clock` is what C's `recGblGetTimeStamp` reads at the read's start.
    pub fn begin_device_cycle(&mut self, dset: &mut impl McaDset, clock: StampSource) -> McaStart {
        let start = self.device_head(dset, clock);
        if start != McaStart::Pending {
            let resume = match start {
                McaStart::Reentered if self.rdns != 0 => Resume::AfterStatus,
                McaStart::Reentered => Resume::AfterData,
                _ => Resume::Nothing,
            };
            self.resume.push(resume);
        }
        start
    }

    /// The rest of the pass [`Self::begin_device_cycle`] began: C's code
    /// after a `send_msg` whose callback already ran, which sees the
    /// `rdns`/`rdng` the nested pass cleared (`:678`, `:777`).
    pub fn finish_device_cycle(&mut self, dset: &mut impl McaDset, clock: StampSource) {
        match self.resume.pop() {
            Resume::Nothing => {}
            Resume::AfterStatus => {
                // C reuses `pstatus`, which the nested pass filled.
                self.apply_status(Ok(self.status));
                let start = self.read_data(dset, clock);
                // The nested passes committed ACQG and cleared READ, so no
                // read is left to start here.
                debug_assert_eq!(start, McaStart::Synchronous);
            }
            // C finds `rdng` clear and reads the data a second time (`:778-788`).
            Resume::AfterData => {
                self.read = 0;
                self.mmap |= M_READ;
                self.read_value(dset);
            }
        }
    }

    /// [`Self::begin_device_cycle`] and [`Self::finish_device_cycle`] in one,
    /// for a device that completes every message inside its send.
    pub fn run_device_cycle(&mut self, dset: &mut impl McaDset, clock: StampSource) {
        let start = self.begin_device_cycle(dset, clock);
        debug_assert_eq!(start, McaStart::Synchronous);
        self.finish_device_cycle(dset, clock);
    }

    /// C `:507-790` from the `reprocess:` label.
    fn device_head(&mut self, dset: &mut impl McaDset, clock: StampSource) -> McaStart {
        if self.nack != 0 {
            debug_print!(1, "process: clearing NACK field\n");
            self.nack = 0;
            self.mmap |= M_NACK;
        }
        debug_print!(
            2,
            "process: entry, rdng={}, rdns={}, read={}\n",
            self.rdng,
            self.rdns,
            self.read
        );
        if self.rdns != 0 {
            // The status callback's pass: `send_msg` copies what it read.
            let status = dset.read_status(self);
            self.apply_status(status);
            return self.read_data(dset, clock);
        }
        if self.rdng == 0 {
            self.send_device_requests(dset);
            debug_print!(5, "process: reading elapsed time, etc.\n");
            match dset.send_read(McaRead::Status) {
                Err(Nack) => self.apply_status(Err(Nack)),
                Ok(Sent::Done) => {
                    let status = dset.read_status(self);
                    self.apply_status(status);
                }
                Ok(sent) => {
                    self.rdns = 1;
                    if sent == Sent::Queued {
                        debug_print!(5, "process: waiting for read status callback.\n");
                        self.mmap |= M_RDNS;
                        return McaStart::Pending;
                    }
                    return McaStart::Reentered;
                }
            }
        }
        self.read_data(dset, clock)
    }

    /// C `read_data:` (`:744-790`).
    pub(crate) fn read_data(&mut self, dset: &mut impl McaDset, clock: StampSource) -> McaStart {
        if self.rdng != 0 {
            debug_print!(5, "process: get data\n");
            self.read_value(dset);
            self.rdng = 0;
            self.mmap |= M_RDNG;
            return McaStart::Synchronous;
        }
        if self.read == 0 {
            return McaStart::Synchronous;
        }
        debug_print!(5, "process: initiate read.\n");
        // C records the time the read BEGAN, for clients computing counts per
        // second across it (`:762-764`).
        self.stamp_asked = true;
        self.rtim = epics_seconds(clock.stamp());
        self.mmap |= M_RTIM;
        let sent = dset.send_read(McaRead::Data);
        if sent == Ok(Sent::Ran) {
            // The nested pass runs inside C's `send_msg`, before `READ` clears.
            self.rdng = 1;
            return McaStart::Reentered;
        }
        self.read = 0;
        self.mmap |= M_READ;
        match sent {
            // A failed send still reads (`:767-783`).
            Err(Nack) => self.not_acknowledged(),
            Ok(Sent::Done) => {}
            Ok(_) => {
                self.rdng = 1;
                debug_print!(5, "process: waiting for read data callback.\n");
                self.mmap |= M_RDNG;
                return McaStart::Pending;
            }
        }
        debug_print!(5, "process: get data\n");
        self.read_value(dset);
        McaStart::Synchronous
    }

    /// C `:509-659` — everything the record does BEFORE the status read.
    ///
    /// The record's own state changes are applied here: `ERAS` zeroes the
    /// spectrum inside the record (`:614`; C's comment at `:612-613` gives the
    /// reason — cheaper than forcing a read back from device support), `STRT`
    /// forces `ACQG` to 1 (`:644`), and every consumed `NEWV` bit is cleared,
    /// so a message goes out once per put.
    pub(crate) fn send_device_requests(&mut self, dset: &mut impl McaDset) {
        // C's NEWV block, in C's field order (`:525-634`). `special()` sets one
        // bit per setup field written; `process` then sends that field's CURRENT
        // value, not the value that was written.
        for (bit, cmd) in [
            (
                NEWV_CHAS,
                McaCommand::ChannelAdvanceSource(self.chas as i32),
            ),
            (NEWV_NUSE, McaCommand::NumChannels(self.nuse)),
            (NEWV_SEQ, McaCommand::Sequence(self.seq)),
            (NEWV_DWEL, McaCommand::DwellTime(self.dwel)),
            (NEWV_PSCL, McaCommand::Prescale(self.pscl)),
            (NEWV_PRTM, McaCommand::PresetRealTime(self.prtm)),
            (NEWV_PLTM, McaCommand::PresetLiveTime(self.pltm)),
            (NEWV_PCT, McaCommand::PresetCounts(self.pct)),
            (NEWV_PCTL, McaCommand::PresetLowChannel(self.pctl)),
            (NEWV_PCTH, McaCommand::PresetHighChannel(self.pcth)),
            (NEWV_PSWP, McaCommand::PresetSweeps(self.pswp)),
        ] {
            if self.newv & bit != 0 {
                // C marks each with its NEWV bit, which aliases a posted field
                // for CHAS (IDTIM) and NUSE (NORD).
                self.mmap |= bit;
                self.send_acknowledged(dset, cmd);
                self.newv &= !bit;
            }
        }

        // ERAS and ERST both erase; only ERAS re-sums the regions. C's comment
        // (`:615-619`): under ERST new data are coming in anyway, so posting the
        // zeroed array is not worth the cost. The ERAS-only arm is `:620-626`.
        if self.newv & (NEWV_ERAS | NEWV_ERST) != 0 {
            self.send_acknowledged(dset, McaCommand::Erase);
            self.nord = 0;
            self.mmap |= M_NORD;
            // NUSE channels, not NMAX (`:614`).
            self.zero_spectrum(self.nuse.max(0) as usize);
            if self.newv & NEWV_ERAS != 0 {
                self.eras = 0;
                // M_ERAS is M_DTIM's bit, so C posts DTIM on every erase.
                self.mmap |= NEWV_ERAS | M_VAL;
                self.newv &= !NEWV_ERAS;
                self.newr = NEWR_ALL;
            }
        }

        if self.newv & NEWV_MODE != 0 {
            self.mmap |= NEWV_MODE;
            if let Some(cmd) = McaCommand::for_mode(self.mode) {
                self.send_acknowledged(dset, cmd);
            }
            self.newv &= !NEWV_MODE;
        }

        // Start and stop go last, so the status read that follows sees them —
        // C's comment at `:636` says so; the start block is `:637-654`. C
        // ignores both sends' status.
        if self.strt != 0 || self.newv & NEWV_ERST != 0 {
            debug_print!(5, "process: start acquisition.\n");
            let _ = dset.send_msg(McaCommand::StartAcquire);
            self.acqg = 1;
            self.mmap |= M_ACQG;
            if self.strt != 0 {
                self.strt = 0;
                self.mmap |= M_STRT;
            }
            if self.newv & NEWV_ERST != 0 {
                self.erst = 0;
                self.newv &= !NEWV_ERST;
                self.mmap |= NEWV_ERST;
            }
        }
        if self.stop != 0 {
            debug_print!(5, "process: stop acquisition.\n");
            let _ = dset.send_msg(McaCommand::StopAcquire);
            self.stop = 0;
            self.mmap |= M_STOP;
        }
    }

    /// C `if (status) {pmca->nack = 1; MARK(M_NACK);}` after a send.
    fn send_acknowledged(&mut self, dset: &mut impl McaDset, cmd: McaCommand) {
        if dset.send_msg(cmd).is_err() {
            self.not_acknowledged();
        }
    }

    fn not_acknowledged(&mut self) {
        self.nack = 1;
        self.mmap |= M_NACK;
    }

    /// C `process` `:666-742` — what the status read MEANS.
    ///
    /// A failed read raises NACK and zeroes `ACQG` unmarked (`:671-674`), and
    /// the cycle goes on with the status read before it, which is what C's
    /// `pstatus` still holds. Acquisition stopping forces a spectrum read
    /// (`:735-742`) so the final spectrum is never missed.
    ///
    /// `ACQG` is deliberately NOT committed here. C's comment at `:736-738`: a
    /// client must not learn that acquisition stopped before the last spectrum
    /// and its ROI sums have been posted. `process()` commits it, after the read.
    pub(crate) fn apply_status(&mut self, status: Result<McaStatus, Nack>) {
        match status {
            Ok(status) => self.status = status,
            Err(Nack) => {
                debug_print!(1, "process: error sending mcaReadStatus\n");
                self.not_acknowledged();
                self.acqg = 0;
            }
        }
        let status = self.status;
        self.rdns = 0;
        debug_print!(
            5,
            "process: status from callback: elapsedReal={:.6}, elapsedLive={:.6}, acquiring={}\n",
            status.elapsed_real,
            status.elapsed_live,
            i32::from(status.acquiring)
        );

        // C's `ertp`/`eltp` start at 0 (`:498`) and take the old value only
        // when that field moved (`:692-701`).
        let (mut prev_ertm, mut prev_eltm) = (0.0, 0.0);
        if self.ertm != status.elapsed_real {
            prev_ertm = self.ertm;
            self.ertm = status.elapsed_real;
            self.mmap |= M_ERTM;
        }
        if self.eltm != status.elapsed_live {
            prev_eltm = self.eltm;
            self.eltm = status.elapsed_live;
            self.mmap |= M_ELTM;
        }
        // C compares the int32 against the double (`:702`), so a fractional
        // count marks ACT on every pass.
        if f64::from(self.act) != status.total_counts {
            self.act = c_cast::f64_to_i32(status.total_counts);
            self.mmap |= M_ACT;
        }
        // Device support is allowed to correct DWEL to the dwell the hardware
        // can actually deliver (`:706-709`).
        if self.dwel != status.dwell_time {
            self.dwel = status.dwell_time;
            self.mmap |= M_DWEL;
        }
        if self.mmap & (M_ERTM | M_ELTM) != 0 {
            self.update_dead_time(prev_ertm, prev_eltm);
            self.mmap |= M_DTIM | M_IDTIM;
        }

        debug_print!(
            5,
            "process: pstatus->acqg={}, acqg={}\n",
            i32::from(status.acquiring),
            self.acqg
        );
        if self.acqg != u16::from(status.acquiring) && !status.acquiring {
            self.read = 1;
            self.mmap |= M_READ;
        }
    }

    /// C `readValue` (`:1097-1115`).
    fn read_value(&mut self, dset: &mut impl McaDset) {
        // Every read marks every region for recomputation (`:1104`).
        self.newr = NEWR_ALL;
        let nord = self.nord;
        match dset.read_array(self) {
            Ok(()) => self.mmap |= M_VAL,
            Err(Nack) => self.not_acknowledged(),
        }
        if self.nord != nord {
            self.mmap |= M_NORD;
        }
    }

    /// Land a spectrum device support read as a value: `VAL` and `NORD`
    /// from it, as C's `read_array` writes `bptr` and `nord`.
    pub fn land_spectrum_read(&mut self, data: EpicsValue) -> CaResult<()> {
        self.land_spectrum(data)
    }

    /// The same read, for a device support that fills the record's buffer IN
    /// PLACE and only reports how many channels it wrote — C's `read_array` is
    /// handed the record, writes through `pmca->bptr`, and sets `pmca->nord`
    /// itself (`devMcaAsyn.c:378-388`, `devMCA_soft.c:155-161`), so "the data"
    /// never crosses the interface as a value.
    ///
    /// `NORD` is clamped to `NMAX`: the buffer is `NMAX` channels deep, and a
    /// driver claiming to have written more is claiming to have written past it.
    pub fn land_channel_count(&mut self, nord: i32) {
        self.nord = nord.clamp(0, self.nmax);
    }

    /// C `mcaRecord.c:710-732`. `DTIM` is the average dead time over the whole acquisition;
    /// `IDTIM` is the dead time over the interval since the previous status read,
    /// and is the field the record ALARMS on. `prev_*` is 0 for a field that
    /// did not change this read, as C's locals are.
    fn update_dead_time(&mut self, prev_ertm: f64, prev_eltm: f64) {
        self.dtim = if self.ertm > 0.001 {
            c_max(
                c_min(100.0 * (self.ertm - self.eltm) / self.ertm, 100.0),
                0.0,
            )
        } else {
            0.0
        };

        self.idtim = if self.acqg != 0 {
            let drt = self.ertm - prev_ertm;
            if drt > 0.001 {
                let dlt = self.eltm - prev_eltm;
                c_max(c_min(100.0 * (drt - dlt) / drt, 100.0), 0.0)
            } else {
                0.0
            }
        } else {
            // Not acquiring: the instantaneous figure has no interval to be
            // instantaneous over, so C reports the average (`:727-729`).
            self.dtim
        };
    }

    /// C `mcaAlarm` (`:962-1003`) — the standard analog ladder, evaluated on
    /// `IDTIM`, not on `VAL`. `VAL` is a spectrum: there is nothing for a limit
    /// to be compared against.
    pub(crate) fn check_dead_time_alarms(&mut self, common: &mut CommonFields) {
        use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
        use epics_base_rs::server::record::AlarmSeverity;

        // C returns before the ladder when the record is undefined (`:968-971`).
        // The framework raises UDF_ALARM itself (`rec_gbl_check_udf`), which is
        // the other half of that same branch.
        if common.udf != 0 {
            return;
        }

        let (idtim, hyst, lalm) = (self.idtim, self.hyst, self.lalm);
        for (limit, severity, status, upper) in [
            (self.hihi, self.hhsv, alarm_status::HIHI_ALARM, true),
            (self.lolo, self.llsv, alarm_status::LOLO_ALARM, false),
            (self.high, self.hsv, alarm_status::HIGH_ALARM, true),
            (self.low, self.lsv, alarm_status::LOW_ALARM, false),
        ] {
            if severity == 0 {
                continue;
            }
            // Once a limit has fired, it keeps firing until IDTIM has left the
            // band by HYST — the standard EPICS latch against a value chattering
            // across a limit.
            let fired = if upper {
                idtim >= limit || (lalm == limit && idtim >= limit - hyst)
            } else {
                idtim <= limit || (lalm == limit && idtim <= limit + hyst)
            };
            if fired {
                // C latches LALM only when `recGblSetSevr` actually RAISED the
                // severity — a limit that lost to a higher pending alarm does not
                // get to move the latch.
                if rec_gbl_set_sevr(common, status, AlarmSeverity::from_u16(severity)) {
                    self.lalm = limit;
                }
                return;
            }
        }
        // Out of every alarm band by at least HYST (`:1001`).
        self.lalm = idtim;
    }
}

/// C `MAX(a,b)` (`mcaRecord.c:177`): `b` unless `a > b`, so a NaN `b`
/// wins and a NaN `a` loses, where `f64::max` drops the NaN either way.
pub(crate) fn c_max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// C `MIN(a,b)` (`mcaRecord.c:178`); see [`c_max`].
pub(crate) fn c_min(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

/// C `recGblGetTimeStamp(pmca)` (`recGbl.c:305-308`) for the mca's stamp
/// points: the record's `TSE` resolved against its `TIME`, as framework
/// context hands them to the record and to its device support.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StampSource {
    pub tse: i16,
    pub time: WallTime,
}

impl Default for StampSource {
    fn default() -> Self {
        Self {
            tse: 0,
            time: WallTime::UNIX_EPOCH,
        }
    }
}

impl From<&ProcessContext> for StampSource {
    fn from(ctx: &ProcessContext) -> Self {
        Self {
            tse: ctx.tse,
            time: ctx.time,
        }
    }
}

impl StampSource {
    /// A TSE whose event has no time keeps the old `TIME`, as C does.
    pub fn stamp(self) -> WallTime {
        get_time_stamp(self.tse, self.time).unwrap_or(self.time)
    }
}

/// C's `pmca->time.secPastEpoch + pmca->time.nsec*1.e-9` (`:763`), with
/// `secPastEpoch` the `epicsUInt32` it is; C's unset `{0, 0}` stamp is 0.
pub(crate) fn epics_seconds(t: WallTime) -> f64 {
    if t == WallTime::UNIX_EPOCH {
        return 0.0;
    }
    let secs = t
        .unix_secs()
        .wrapping_sub(epics_base_rs::runtime::general_time::EPICS_EPOCH_UNIX_SECS)
        as u32;
    f64::from(secs) + f64::from(t.subsec_nanos()) * 1.0e-9
}

/// C `:812-814`, `epicsTimeToStrftime(pmca->stim, 25, "%b %d, %Y
/// %H:%M:%S.%03f", &pmca->time)`, written over `stim`'s own buffer.
///
/// The prefix renders 22 bytes, leaving 3 of the 25 — too few for three
/// digits and the NUL — so C's no-room branch (`epicsTime.cpp:265-276`)
/// writes `bufLenLeft - 1` stars instead: `Jul 14, 2026 19:28:45.**`. The
/// `stim[25]` trim at `:814` then has nothing to cut.
pub(crate) fn format_stim(stim: &mut PvString, t: WallTime) {
    use chrono::{Datelike, Timelike};
    use std::fmt::Write;

    const BUF_LENGTH: usize = 25;
    const FRAC_DIGITS: usize = 3;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    // C's `{0, 0}` stamp (`epicsTime.cpp:176-180`).
    if t == WallTime::UNIX_EPOCH {
        stim.set_bytes(b"<undefined>");
        return;
    }
    let dt: chrono::DateTime<chrono::Local> = SystemTime::from(t).into();
    let mut out = Bounded::<BUF_LENGTH>::default();
    // `strftime` writes nothing for a prefix that does not fit.
    if write!(
        out,
        "{} {:02}, {} {:02}:{:02}:{:02}.",
        MONTHS[dt.month0() as usize],
        dt.day(),
        dt.year(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
    .is_err()
    {
        out.len = 0;
    }
    let left = BUF_LENGTH - out.len;
    if left > 1 {
        if FRAC_DIGITS < left {
            // `nsec + div/2`, clamped below a whole second, over `div`.
            let frac = (t.subsec_nanos() + 500_000).min(999_999_999) / 1_000_000;
            let _ = write!(out, "{frac:03}");
        } else {
            let _ = out.write_str(&"************"[..left - 1]);
        }
    }
    stim.set_bytes(&out.buf[..out.len]);
}

/// A `char[N]` for `fmt::Write`: holds at most `N - 1` bytes, the NUL's slot
/// kept free, and refuses a write that would not fit.
struct Bounded<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Default for Bounded<N> {
    fn default() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }
}

impl<const N: usize> std::fmt::Write for Bounded<N> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end >= N {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

// C `mcaRecord.c:224-240` — the MMAP bits of the fields only `process()`
// marks. The NEWV bits below share MMAP, and three of them alias these:
// M_ERAS is M_DTIM, M_CHAS is M_IDTIM, M_NUSE is M_NORD.
pub(crate) const M_VAL: u32 = 0x0000_0001;
pub(crate) const M_NACK: u32 = 0x0000_0002;
pub(crate) const M_BG: u32 = 0x0000_0004;
pub(crate) const M_ACQG: u32 = 0x0000_0008;
pub(crate) const M_READ: u32 = 0x0000_0010;
pub(crate) const M_RDNG: u32 = 0x0000_0020;
pub(crate) const M_ERTM: u32 = 0x0000_0040;
pub(crate) const M_ELTM: u32 = 0x0000_0080;
pub(crate) const M_ACT: u32 = 0x0000_0100;
pub(crate) const M_STRT: u32 = 0x0000_0200;
pub(crate) const M_STOP: u32 = 0x0000_0400;
pub(crate) const M_STIM: u32 = 0x0000_0800;
pub(crate) const M_RTIM: u32 = 0x0000_1000;
pub(crate) const M_RDNS: u32 = 0x0000_2000;
pub(crate) const M_DTIM: u32 = 0x0000_4000;
pub(crate) const M_IDTIM: u32 = 0x0000_8000;
pub(crate) const M_NORD: u32 = 0x0001_0000;
pub(crate) const M_DWEL: u32 = 0x0002_0000;

// C `mcaRecord.c:242-256` — the NEWV bits. `NEWV` is a `DBF_ULONG` field and so
// is CA-visible: the bit VALUES are part of the field surface, not an
// implementation detail.
pub(crate) const NEWV_ERAS: u32 = 0x0000_4000;
pub(crate) const NEWV_CHAS: u32 = 0x0000_8000;
pub(crate) const NEWV_NUSE: u32 = 0x0001_0000;
pub(crate) const NEWV_DWEL: u32 = 0x0002_0000;
pub(crate) const NEWV_PRTM: u32 = 0x0004_0000;
pub(crate) const NEWV_PLTM: u32 = 0x0008_0000;
pub(crate) const NEWV_PCT: u32 = 0x0010_0000;
pub(crate) const NEWV_PCTL: u32 = 0x0020_0000;
pub(crate) const NEWV_PCTH: u32 = 0x0040_0000;
pub(crate) const NEWV_PSWP: u32 = 0x0080_0000;
pub(crate) const NEWV_MODE: u32 = 0x0100_0000;
pub(crate) const NEWV_SEQ: u32 = 0x0200_0000;
pub(crate) const NEWV_PSCL: u32 = 0x0400_0000;
pub(crate) const NEWV_ERST: u32 = 0x0800_0000;

/// C `M_ROI_ALL` (`:295`) — every region marked for recomputation.
pub(crate) const NEWR_ALL: u32 = 0xFFFF_FFFF;
