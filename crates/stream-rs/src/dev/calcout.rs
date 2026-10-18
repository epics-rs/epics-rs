//! `devcalcoutStream.c`: the StreamDevice interface for calcout records.

use std::cmp::Ordering::{Equal, Less};

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::c_cast::{f64_to_c_long, f64_to_u64};

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(calcout,INST_IO,devcalcoutStream,"stream")`: six entries,
/// `report` (`streamReport` is NULL) and `special_linconv` NULL.
pub static DEV_CALCOUT_STREAM: DevSup = DevSup {
    name: "devcalcoutStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` calcout's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Calcout)
}

fn calcout(record: &mut dyn Record) -> &mut CalcoutRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devcalcoutStream is bound to calcout records only")
}

/// C's `!(fabs(last - val) <= del)`: a NaN difference passes.
fn outside(last: f64, val: f64, del: f64) -> bool {
    !matches!((last - val).abs().partial_cmp(&del), Some(Less | Equal))
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let co = calcout(record);

    match format.type_ {
        Dbf::Double => {
            if io.scanf_double(format, &mut co.val) == ERROR {
                return ERROR;
            }
        }
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut lval = 0;
            if io.scanf_long(format, &mut lval) == ERROR {
                return ERROR;
            }
            co.val = if format.type_ == Dbf::Long {
                lval as f64
            } else {
                lval as u64 as f64
            };
        }
        _ => return ERROR,
    }
    if io.pact() {
        return OK;
    }
    // In @init handler, no processing, enforce monitor updates.
    let reset = rec_gbl_reset_alarms(common);
    io.post_alarm_reset(common, &reset);
    let mut monitor_mask = if reset.alarm_changed || reset.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };
    if outside(co.mlst, co.val, co.mdel) {
        monitor_mask |= EventMask::VALUE;
        co.mlst = co.val;
    }
    // C bug not copied: devcalcoutStream.c:62 tests ADEL against `mlst`,
    // which the MDEL test may just have set to VAL, so a change that posts
    // DBE_VALUE never posts DBE_LOG.
    if outside(co.alst, co.val, co.adel) {
        monitor_mask |= EventMask::LOG;
        co.alst = co.val;
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::Double(co.val), monitor_mask);
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let co = calcout(record);

    match format.type_ {
        Dbf::Double => io.printf_double(format, co.oval),
        Dbf::Ulong => io.printf_long(format, f64_to_u64(co.oval) as i64),
        Dbf::Long | Dbf::Enum => io.printf_long(format, f64_to_c_long(co.oval)),
        _ => ERROR,
    }
}

struct Calcout;

impl StreamRecordType for Calcout {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            map_error: true,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}
