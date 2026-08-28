//! `devMCA_soft` — the mca record's soft-channel device support.
//!
//! C: `mca/mcaApp/mcaSrc/devMCA_soft.c`, declared `device(mca, CONSTANT,
//! devMCA_soft, "Soft Channel")`.
//!
//! There is no hardware behind it. Every `send_msg` case is an empty `case` with
//! a debug print (`:74-153`), `read_array` sets `pmca->nord = pmca->nuse` and
//! returns without touching a single channel (`:155-161`), and the `mcaStatus`
//! struct the record hands to `mcaReadStatus` is never written — so the record
//! reads back a status of all zeroes. The spectrum a soft mca serves is whatever
//! a `.db` or a client put into `VAL`, which is what makes it useful: it is the
//! ROI machinery, on a spectrum you supply.
//!
//! Tier 3 (correctness only, no bug-for-bug parity), with one deviation from C
//! called out below.

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use crate::record::{McaCommand, McaRecord, McaStatus};

/// `device(mca, CONSTANT, devMCA_soft, "Soft Channel")`.
#[derive(Debug, Default)]
pub struct SoftMca;

impl SoftMca {
    fn mca(record: &mut dyn Record) -> CaResult<&mut McaRecord> {
        record
            .as_any_mut()
            .and_then(|any| any.downcast_mut::<McaRecord>())
            .ok_or_else(|| {
                epics_base_rs::error::CaError::TypeMismatch(
                    "DTYP \"Soft Channel\" (devMCA_soft) supports the mca record only".into(),
                )
            })
    }
}

impl DeviceSupport for SoftMca {
    fn dtyp(&self) -> &str {
        "Soft Channel"
    }

    /// C `init_record` (`devMCA_soft.c:64-72`): `pmca->nord = 0`.
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Self::mca(record)?.nord = 0;
        Ok(DeviceInitOutcome::Live)
    }

    /// The device half of one process cycle: send what the record asked to send,
    /// read the status back, and read the spectrum if the record wants it.
    ///
    /// The three steps ARE C's `process`, in C's order — see [`crate::record::cycle`]
    /// for why they are invoked from here rather than from `Record::process`.
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let mca = Self::mca(record)?;

        // C `send_msg`: every command is a no-op that returns 0, so NACK is never
        // raised and no record state changes beyond what the record itself
        // applied when it handed the commands over.
        for cmd in mca.take_device_requests() {
            let _: McaCommand = cmd;
        }

        // C hands `pmca->pstatus` to `send_msg(mcaReadStatus, ...)`, which writes
        // nothing (`:133-140`), so the record reads back a status of all zeroes —
        // and `acquiring = 0` in particular. A soft mca therefore never acquires:
        // a `STRT` forces `ACQG` to 1, the zeroed status reports 0, the record
        // sees the 1 -> 0 edge and reads the spectrum, and `ACQG` is back to 0 by
        // the end of that same cycle. That is not an accident of the stub, it is
        // what a device with no acquisition clock CAN report, so the port keeps
        // it.
        //
        // TIER-3 DEVIATION (a driver's tier is correctness, not parity): C's
        // zeroed status also carries `dwellTime = 0`, and the record copies it
        // (`mcaRecord.c:706-709`) — so the first process of a soft mca silently
        // destroys `DWEL`. Measured on the C IOC built for this port: `DWEL`
        // reads 1 at iocInit and 0 after a single `caput PROC 1`, on a record
        // whose `.db` never mentions `DWEL`. A device that cannot correct the
        // dwell time reports the dwell it was given, not zero.
        let landed = mca.apply_status(McaStatus {
            dwell_time: mca.dwel,
            ..Default::default()
        });

        if landed {
            // C `read_array` (`devMCA_soft.c:155-161`): the buffer is left as it is —
            // the soft device produces no data — and only NORD moves.
            mca.land_channel_count(mca.nuse);
        }

        Ok(DeviceReadOutcome::ok())
    }

    /// An mca has no output: C's dset carries `send_msg` and `read_array`, and no
    /// write entry at all.
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    // `handle_command` is deliberately not implemented. The record's one
    // out-of-band command is the preset stop, and C's `send_msg(mcaStopAcquire)`
    // is an empty `case` (`devMCA_soft.c:141-144`): a device with no acquisition
    // has none to stop. The framework's default no-op is exactly right, and an
    // override that poked `ACQG` would make the DEVICE a second writer of state
    // the record already owns.
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::types::EpicsValue;

    fn soft_mca(nmax: i32) -> (SoftMca, McaRecord) {
        let mut rec = McaRecord {
            nmax,
            nuse: nmax,
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();
        let mut dev = SoftMca;
        dev.init(&mut rec).unwrap();
        (dev, rec)
    }

    /// One process cycle, the way the framework runs it: device support's `read`,
    /// then the record's `process`.
    fn cycle(dev: &mut SoftMca, rec: &mut McaRecord) {
        dev.read(rec).unwrap();
        for action in rec.process().unwrap().actions {
            if let epics_base_rs::server::record::ProcessAction::DeviceCommand { command, args } =
                action
            {
                dev.handle_command(rec, command, &args).unwrap();
            }
        }
    }

    /// C's soft support zeroes the record's `DWEL` on the first process, because
    /// it reports a status struct it never filled and the record copies the
    /// dwell time out of it. Measured on the C IOC: `DWEL` is 1 at iocInit and 0
    /// after one `caput PROC 1`. A device that cannot correct the dwell reports
    /// the dwell it was given.
    #[test]
    fn the_soft_device_does_not_destroy_the_dwell_time() {
        let (mut dev, mut rec) = soft_mca(16);
        rec.put_field("DWEL", EpicsValue::Double(0.5)).unwrap();
        rec.special("DWEL", true).unwrap();
        cycle(&mut dev, &mut rec);
        assert_eq!(rec.dwel, 0.5);
    }

    /// C `read_array` sets `NORD = NUSE` and writes no channels: the spectrum a
    /// soft mca serves is the one a client put there.
    #[test]
    fn a_read_publishes_nuse_channels_of_whatever_is_in_the_buffer() {
        let (mut dev, mut rec) = soft_mca(16);
        rec.put_field("VAL", EpicsValue::LongArray(vec![7; 16]))
            .unwrap();
        rec.put_field("NUSE", EpicsValue::Long(4)).unwrap();
        rec.put_field("READ", EpicsValue::Enum(1)).unwrap();

        cycle(&mut dev, &mut rec);
        assert_eq!(rec.nord, 4);
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::LongArray(vec![7, 7, 7, 7]))
        );
    }

    /// A soft mca cannot acquire — its status has no acquisition clock in it —
    /// so a `STRT` completes within the cycle it was issued in: `ACQG` is forced
    /// to 1 by the start, the zeroed status reports 0, the record sees the
    /// 1 -> 0 edge, reads the spectrum, and publishes `ACQG = 0`.
    ///
    /// Measured on the C IOC: `TEST:mca1.ACQG` reads `Done` after
    /// `caput TEST:mca1.STRT 1`.
    #[test]
    fn a_start_completes_within_the_cycle_and_the_spectrum_is_read() {
        let (mut dev, mut rec) = soft_mca(16);
        rec.put_field("VAL", EpicsValue::LongArray(vec![3; 16]))
            .unwrap();
        rec.nord = 0;

        rec.put_field("STRT", EpicsValue::Enum(1)).unwrap();
        cycle(&mut dev, &mut rec);

        assert_eq!(rec.acqg, 0);
        assert_eq!(rec.nord, 16, "the 1 -> 0 edge forced the read");
        assert_eq!(rec.strt, 0, "and the start is consumed");
    }

    /// The regions are summed on a soft mca like any other: the spectrum is
    /// whatever the client put in `VAL`, and the sums follow it.
    #[test]
    fn the_regions_are_summed_over_the_spectrum_a_client_supplied() {
        let (mut dev, mut rec) = soft_mca(16);
        rec.put_field("VAL", EpicsValue::LongArray(vec![10; 16]))
            .unwrap();
        rec.roi[0] = crate::record::Roi {
            lo: 0,
            hi: 3,
            nbg: -1,
            ..Default::default()
        };
        rec.put_field("READ", EpicsValue::Enum(1)).unwrap();

        cycle(&mut dev, &mut rec);
        assert_eq!(rec.roi[0].sum, 40.0, "4 channels of 10");
        assert_eq!(rec.roi[0].net, 40.0, "no background");
    }
}
