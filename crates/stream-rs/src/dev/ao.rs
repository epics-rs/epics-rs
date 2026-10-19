//! `devaoStream.c`: the StreamDevice interface for ao records.

use std::cmp::Ordering::{Equal, Less};

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::c_cast::{f64_to_c_long, f64_to_u64};

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(ao,INST_IO,devaoStream,"stream")`: six entries, `report`
/// (`streamReport` is NULL) and `special_linconv` NULL.
pub static DEV_AO_STREAM: DevSup = DevSup {
    name: "devaoStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` ao's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Ao)
}

fn ao(record: &mut dyn Record) -> &mut AoRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devaoStream is bound to ao records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let ao = ao(record);
    let mut val;

    match format.type_ {
        Dbf::Double => {
            val = 0.0;
            if io.scanf_double(format, &mut val) == ERROR {
                return ERROR;
            }
        }
        Dbf::Ulong | Dbf::Long => {
            let mut rval = 0;
            if io.scanf_long(format, &mut rval) == ERROR {
                return ERROR;
            }
            ao.rbv = rval as i32;
            ao.rval = rval as i32;
            val = if format.type_ == Dbf::Ulong {
                rval as u64 as f64
            } else {
                rval as f64
            };
            // devaoStream.c:51-62, the ROFF and LINR conversion after this
            // break, is unreachable since dace4e5: VAL is x*ASLO+AOFF.
        }
        _ => return ERROR,
    }
    if ao.aslo != 0.0 {
        val *= ao.aslo;
    }
    val += ao.aoff;
    ao.val = val;
    if io.pact() {
        return DO_NOT_CONVERT;
    }
    // In @init handler, no processing, enforce monitor updates.
    ao.omod = ao.oval != val;
    // C also sets `orbv = (epicsInt32)(oval = val)` (devaoStream.c:73), so
    // RBV posted when it differed from (int)VAL, not when it changed: ORBV
    // stays the old RBV here.
    ao.oval = val;
    let reset = rec_gbl_reset_alarms(common);
    io.post_alarm_reset(common, &reset);
    let mut monitor_mask = if reset.alarm_changed || reset.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };
    // a NaN difference passes, as C's `!(fabs(...) <= mdel)`
    if !matches!(
        (ao.mlst - val).abs().partial_cmp(&ao.mdel),
        Some(Less | Equal)
    ) {
        monitor_mask |= EventMask::VALUE;
        ao.mlst = val;
    }
    if !matches!(
        (ao.alst - val).abs().partial_cmp(&ao.adel),
        Some(Less | Equal)
    ) {
        monitor_mask |= EventMask::LOG;
        ao.alst = val;
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::Double(ao.val), monitor_mask);
    }
    if ao.omod {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
    }
    if !monitor_mask.is_empty() {
        ao.omod = false;
        io.post_events("OVAL", EpicsValue::Double(ao.oval), monitor_mask);
        if ao.oraw != ao.rval {
            let mask = monitor_mask | EventMask::VALUE | EventMask::LOG;
            io.post_events("RVAL", EpicsValue::Long(ao.rval), mask);
            ao.oraw = ao.rval;
        }
        if ao.orbv != ao.rbv {
            let mask = monitor_mask | EventMask::VALUE | EventMask::LOG;
            io.post_events("RBV", EpicsValue::Long(ao.rbv), mask);
            ao.orbv = ao.rbv;
        }
    }
    DO_NOT_CONVERT
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let ao = ao(record);

    let mut val = if io.init_run() { ao.val } else { ao.oval } - ao.aoff;
    if ao.aslo != 0.0 && ao.aslo != 1.0 {
        val /= ao.aslo;
    }

    match format.type_ {
        Dbf::Double => io.printf_double(format, val),
        Dbf::Ulong => {
            if ao.linr == 0 {
                // allow integers with more than 32 bits
                return io.printf_long(format, f64_to_u64(val) as i64);
            }
            // (unsigned long) of an epicsInt32 sign-extends: the long's bits
            io.printf_long(format, ao.rval as i64)
        }
        Dbf::Long => {
            if ao.linr == 0 {
                // allow integers with more than 32 bits
                return io.printf_long(format, f64_to_c_long(val));
            }
            io.printf_long(format, ao.rval as i64)
        }
        _ => ERROR,
    }
}

struct Ao;

impl StreamRecordType for Ao {
    /// C returns `streamInitRecord`'s status as it is, unlike ai's.
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}
