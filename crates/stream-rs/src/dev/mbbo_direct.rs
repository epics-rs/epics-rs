//! `devmbboDirectStream.c`: the StreamDevice interface for mbboDirect
//! records.

use epics_base_rs::server::database::HeldInstance;
use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, DbLinkType, DevSup, Dset, Record,
};
use epics_base_rs::server::records::mbbi_direct::BIT_NAMES;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, Stream, StreamDevice, StreamInitArgs, StreamIo,
    StreamRecordType,
};

/// `device(mbboDirect,INST_IO,devmbboDirectStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_MBBO_DIRECT_STREAM: DevSup = DevSup {
    name: "devmbboDirectStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` mbboDirect's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(MbboDirect)
}

fn mbbo_direct(record: &mut dyn Record) -> &mut MbboDirectRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devmbboDirectStream is bound to mbboDirect records only")
}

/// The record's field as it serves it.
fn post(io: &StreamIo<'_>, mbbo_d: &MbboDirectRecord, field: &'static str, mask: EventMask) {
    if let Some(value) = Record::get_field(mbbo_d, field) {
        io.post_events(field, value, mask);
    }
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbo_d = mbbo_direct(record);

    if format.type_ != Dbf::Ulong && format.type_ != Dbf::Long {
        return ERROR;
    }
    let mut v = 0;
    if io.scanf_long(format, &mut v) == ERROR {
        return ERROR;
    }
    let mut val = v as u64;
    if mbbo_d.mask != 0 {
        val &= mbbo_d.mask as u64;
    }

    mbbo_d.rbv = val as u32;
    mbbo_d.rval = val as u32;
    // C's shift is undefined from 64 on; 0 is the fully shifted-out value.
    val = val.checked_shr(mbbo_d.shft as u32).unwrap_or(0);
    mbbo_d.val = val as u32;

    if io.pact() {
        return DO_NOT_CONVERT;
    }
    // In @init handler, no processing, enforce monitor updates.
    let reset = rec_gbl_reset_alarms(common);
    io.post_alarm_reset(common, &reset);
    let mut monitor_mask = if reset.alarm_changed || reset.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };
    if mbbo_d.mlst != mbbo_d.val {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
        mbbo_d.mlst = mbbo_d.val;
    }
    if !monitor_mask.is_empty() {
        post(io, mbbo_d, "VAL", monitor_mask);
    }
    let changed = monitor_mask | EventMask::VALUE | EventMask::LOG;
    if mbbo_d.oraw != mbbo_d.rval {
        post(io, mbbo_d, "RVAL", changed);
        mbbo_d.oraw = mbbo_d.rval;
    }
    if mbbo_d.orbv != mbbo_d.rbv {
        post(io, mbbo_d, "RBV", changed);
        mbbo_d.orbv = mbbo_d.rbv;
    }
    // update the bits
    for (i, name) in BIT_NAMES.iter().enumerate() {
        let bit = (val & 1) as u8;
        if bit == (mbbo_d.bits[i] == 0) as u8 {
            mbbo_d.bits[i] = bit;
            post(io, mbbo_d, name, changed);
        } else if !monitor_mask.is_empty() {
            post(io, mbbo_d, name, monitor_mask);
        }
        val >>= 1;
    }
    DO_NOT_CONVERT
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbo_d = mbbo_direct(record);

    let val = match format.type_ {
        Dbf::Ulong => {
            let mut val = mbbo_d.rval as i64;
            if mbbo_d.mask != 0 {
                val &= mbbo_d.mask as i64;
            }
            val
        }
        Dbf::Long | Dbf::Enum => {
            let mut val = mbbo_d.rval as i32 as i64;
            if mbbo_d.mask != 0 {
                val &= mbbo_d.mask as i32 as i64;
            }
            val
        }
        _ => return ERROR,
    };
    io.printf_long(format, val)
}

struct MbboDirect;

impl StreamRecordType for MbboDirect {
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        let m = mbbo_direct(record);
        // C's `mbboD->mask <<= mbboD->shft` is undefined for a SHFT of 32 or
        // more; 0 is the fully shifted-out mask, as the record's RVAL shift.
        m.mask = m.mask.checked_shl(m.shft as u32).unwrap_or(0);

        // Workaround for bug in mbboDirect record:
        // Put to VAL overwrites value to 0 if SEVR is INVALID_ALARM
        // Thus first write may send a wrong value.
        common.sevr = AlarmSeverity::NoAlarm;
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            // devmbboDirectStream.c:114 returns streamInitRecord's status unmapped.
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        true
    }

    /// `write_mbbo`: the bug also corrupts the next write to VAL after an
    /// I/O error, so the record is never left in INVALID_ALARM.
    fn read_write(&self, stream: &Stream, held: &HeldInstance<'_>) -> i64 {
        let status = stream.read_write(held);
        held.with(|_, common| {
            if common.nsev == AlarmSeverity::Invalid {
                common.nsev = AlarmSeverity::Major;
            }
        });
        status
    }
}
