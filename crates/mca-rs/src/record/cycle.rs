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
//! A port record cannot call its device support — that is what `ProcessAction`
//! and the `DeviceSupport` trait exist to prevent — and the framework calls
//! `DeviceSupport::read()` BEFORE `Record::process()`. So the cycle is split at
//! the two points where C's control flow crosses into the driver, and each half
//! is a method ON THE RECORD:
//!
//! - C `:510-659` (clear NACK, drain NEWV, ERAS/ERST, STRT, STOP) is
//!   [`McaRecord::take_device_requests`]: the record applies its own state
//!   changes and returns the commands to send, in C's order.
//! - C `:666-742` (status read, ERTM/ELTM/ACT/DWEL, dead time, force a read when
//!   acquisition stops) is [`McaRecord::apply_status`]: device support performs
//!   the read; the record decides what the status MEANS and whether the spectrum
//!   must now be read.
//! - C `:744-790` (the data read) is device support's, landing through
//!   [`McaRecord::land_spectrum_read`].
//! - C `:793-841` (ROI sums, preset stop, commit ACQG, UDF, STIM, alarms,
//!   forward link) is the record's `process()`.
//!
//! The record therefore remains the only writer of its own state, and device
//! support remains the only thing that touches hardware. What moved is where the
//! device I/O is INVOKED from, not who decides what it means.

use std::time::SystemTime;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::CommonFields;
use epics_base_rs::types::EpicsValue;

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
    ///
    /// **Tier-2 deviation.** C never sends `mcaAcquireMode`: it sends the MODE
    /// VALUE ITSELF as the command code — `(*pdset->send_msg)(pmca, pmca->mode,
    /// NULL)` (`mcaRecord.c:630`). So `caput MODE MCS` (index 1) reaches device
    /// support as command 1, which is `mcaStartAcquire`, and `caput MODE List`
    /// (index 2) arrives as `mcaStopAcquire`; `mcaAcquireMode` is command 8, and
    /// no MODE write can ever produce it. That is a collision between two enums
    /// that happen to share a numbering, not a contract. The port sends the mode
    /// as a mode command carrying the index.
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

impl McaRecord {
    /// C `process` `mcaRecord.c:510-659` — everything the record does BEFORE the status
    /// read, and the commands that must reach the device ahead of it.
    ///
    /// The record's own state changes are applied here, not by the caller:
    /// `ERAS` zeroes the spectrum inside the record (`:614`; C's comment at
    /// `:612-613` gives the reason — cheaper than forcing a read back from
    /// device support), `STRT` forces `ACQG` to 1 (`:644`), and every consumed
    /// `NEWV` bit is cleared. So this is the SOLE consumer of
    /// `NEWV`/`STRT`/`STOP`/`ERAS`/`ERST`: calling it twice for one cycle cannot
    /// double-send, because the flags are already gone.
    pub fn take_device_requests(&mut self) -> Vec<McaCommand> {
        let mut cmds = Vec::new();
        self.nack = 0;

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
                cmds.push(cmd);
                self.newv &= !bit;
            }
        }

        // ERAS and ERST both erase; only ERAS re-sums the regions. C's comment
        // (`:615-619`): under ERST new data are coming in anyway, so posting the
        // zeroed array is not worth the cost. The ERAS-only arm is `:620-626`.
        if self.newv & (NEWV_ERAS | NEWV_ERST) != 0 {
            cmds.push(McaCommand::Erase);
            self.nord = 0;
            // NUSE channels, not NMAX (`:614`).
            self.zero_spectrum(self.nuse.max(0) as usize);
            if self.newv & NEWV_ERAS != 0 {
                self.eras = 0;
                self.newv &= !NEWV_ERAS;
                self.newr = NEWR_ALL;
            }
        }

        if self.newv & NEWV_MODE != 0 {
            cmds.push(McaCommand::AcquireMode(self.mode as i32));
            self.newv &= !NEWV_MODE;
        }

        // Start and stop go last, so the status read that follows sees them —
        // C's comment at `:636` says so; the start block is `:637-654`.
        if self.strt != 0 || self.newv & NEWV_ERST != 0 {
            cmds.push(McaCommand::StartAcquire);
            self.acqg = 1;
            self.strt = 0;
            if self.newv & NEWV_ERST != 0 {
                self.erst = 0;
                self.newv &= !NEWV_ERST;
            }
        }
        if self.stop != 0 {
            cmds.push(McaCommand::StopAcquire);
            self.stop = 0;
        }

        cmds
    }

    /// C `process` `:687-742` — what the status device support just read MEANS.
    ///
    /// Returns `true` when the spectrum must now be read: either a client set
    /// `READ`, or acquisition has just stopped — C forces a read on the 1 -> 0
    /// transition (`:735-742`) so the final spectrum is never missed.
    ///
    /// `ACQG` is deliberately NOT committed here. C's comment at `:736-738`: a
    /// client must not learn that acquisition stopped before the last spectrum
    /// and its ROI sums have been posted. `process()` commits it, after the read.
    pub fn apply_status(&mut self, status: McaStatus) -> bool {
        self.rdns = 0;

        let times_moved = self.ertm != status.elapsed_real || self.eltm != status.elapsed_live;
        let (prev_ertm, prev_eltm) = (self.ertm, self.eltm);
        self.ertm = status.elapsed_real;
        self.eltm = status.elapsed_live;
        self.act = status.total_counts as i32;
        // Device support is allowed to correct DWEL to the dwell the hardware
        // can actually deliver (`:706-709`).
        self.dwel = status.dwell_time;
        if times_moved {
            self.update_dead_time(prev_ertm, prev_eltm);
        }

        self.status = status;
        if self.acqg != u16::from(status.acquiring) && !status.acquiring {
            self.read = 1;
        }

        if self.read == 0 {
            return false;
        }
        self.read = 0;
        // C records the time the read BEGAN, for clients computing counts per
        // second across it (`:762-764`).
        self.rtim = epics_seconds(SystemTime::now());
        true
    }

    /// C `readValue` (`:1097-1132`) once device support has produced the data:
    /// land it, and mark every region for recomputation.
    pub fn land_spectrum_read(&mut self, data: EpicsValue) -> CaResult<()> {
        self.read_completed();
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
        self.read_completed();
        self.nord = nord.clamp(0, self.nmax);
    }

    /// C `readValue`'s first statement: EVERY read marks every region for
    /// recomputation (`mcaRecord.c:1104`) — the spectrum under them has just
    /// changed. Qualified, not bare: the nearest preceding file mention in this
    /// module is `devMCA_soft.c`, which is 162 lines long, so a bare `:1104`
    /// here reads as a past-EOF error while naming a line that exists.
    fn read_completed(&mut self) {
        self.newr = NEWR_ALL;
        self.rdng = 0;
    }

    /// C `mcaRecord.c:710-732`. `DTIM` is the average dead time over the whole acquisition;
    /// `IDTIM` is the dead time over the interval since the previous status read,
    /// and is the field the record ALARMS on.
    ///
    /// **Tier-2 deviation — C's interval is not an interval.** C keeps the
    /// previous elapsed times in two locals initialised to zero at the top of
    /// `process` (`:498`, `double ertp=0., eltp=0.;`) and assigns them only
    /// inside the branch that saw that field CHANGE (`:692-701`). So on a cycle
    /// where the real time moved but the live time did not — a detector so dead
    /// that its live-time clock has stopped, which is precisely the condition
    /// `IDTIM` exists to report — C evaluates `100*(drt - (eltm - 0))/drt`
    /// instead of `100*(drt - 0)/drt`, gets a large negative number, clamps it to
    /// 0, and reports 0% instantaneous dead time at the moment the detector is
    /// 100% dead. The port passes the actual previous values, so the interval is
    /// the interval.
    fn update_dead_time(&mut self, prev_ertm: f64, prev_eltm: f64) {
        self.dtim = if self.ertm > 0.001 {
            (100.0 * (self.ertm - self.eltm) / self.ertm).clamp(0.0, 100.0)
        } else {
            0.0
        };

        self.idtim = if self.acqg != 0 {
            let drt = self.ertm - prev_ertm;
            if drt > 0.001 {
                let dlt = self.eltm - prev_eltm;
                (100.0 * (drt - dlt) / drt).clamp(0.0, 100.0)
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

/// Seconds past the EPICS epoch — C's `pmca->time.secPastEpoch +
/// pmca->time.nsec*1.e-9` (`:763`).
pub(crate) fn epics_seconds(t: SystemTime) -> f64 {
    let unix = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    unix - epics_base_rs::runtime::general_time::EPICS_EPOCH_UNIX_SECS as f64
}

/// C `:809-816` — the wall-clock time acquisition stopped, to millisecond
/// precision.
///
/// **Tier-2 deviation — C truncates its own output.** C renders `"%b %d, %Y
/// %H:%M:%S.%03f"` into `pmca->stim` with `epicsTimeToStrftime(pmca->stim, 25,
/// ...)`. That rendering needs 26 bytes (`Jul 14, 2026 19:28:45.123` is 25
/// characters, plus the NUL); given 25, `epicsTimeToStrftime` cannot fit the
/// fractional field and writes its overflow marker instead, so what a client
/// reads back ends `.**`. Measured on the C IOC built for this port: `STIM =
/// "Jul 14, 2026 19:28:45.**"`. The `.dbd` declares `STIM` as `size(40)`, so the
/// buffer was never the constraint — the hard-coded 25 was — and the line
/// immediately after it (`pmca->stim[25]='\0'`, commented "Trim STIM to 25
/// characters = .001 sec precision") says exactly what the intent was. The port
/// emits the milliseconds.
pub(crate) fn format_stim(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%b %d, %Y %H:%M:%S%.3f").to_string()
}

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
