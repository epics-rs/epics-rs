//! `devboStream.c`: the StreamDevice interface for bo records.

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(bo,INST_IO,devboStream,"stream")`: five entries, `report`
/// (`streamReport` is NULL) not set.
pub static DEV_BO_STREAM: DevSup = DevSup {
    name: "devboStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// `sizeof(bo->znam)`, boRecord.dbd's `size(26)`.
const ZNAM_SIZE: usize = 26;

/// A `DTYP "stream"` bo's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Bo)
}

fn bo(record: &mut dyn Record) -> &mut BoRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devboStream is bound to bo records only")
}

/// A `char[]` up to its first 0x00, as `strcmp` reads it.
fn c_str(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let bo = bo(record);
    // C's unsigned long: VAL tests all 64 bits, RBV and RVAL keep 32
    let mut val = 0;

    match format.type_ {
        Dbf::Ulong | Dbf::Long => {
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            if bo.mask != 0 {
                val &= bo.mask as i64;
            }
            bo.rbv = val as u32;
            bo.rval = val as u32;
        }
        Dbf::Enum => {
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
        }
        Dbf::String => {
            let mut buffer = [0u8; ZNAM_SIZE];
            if io.scanf_string(format, &mut buffer) == ERROR as isize {
                return ERROR;
            }
            let buffer = c_str(&buffer);
            if c_str(bo.znam.as_bytes()) == buffer {
                val = 0;
            } else if c_str(bo.onam.as_bytes()) == buffer {
                val = 1;
            } else {
                return ERROR;
            }
        }
        _ => return ERROR,
    }
    bo.val = (val != 0) as u16;
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
    if bo.mlst != bo.val {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
        bo.mlst = bo.val;
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::Enum(bo.val), monitor_mask);
    }
    // RVAL and RBV post on a change whatever VAL did
    if bo.oraw != bo.rval {
        let mask = monitor_mask | EventMask::VALUE | EventMask::LOG;
        io.post_events("RVAL", EpicsValue::ULong(bo.rval), mask);
        bo.oraw = bo.rval;
    }
    if bo.orbv != bo.rbv {
        let mask = monitor_mask | EventMask::VALUE | EventMask::LOG;
        io.post_events("RBV", EpicsValue::ULong(bo.rbv), mask);
        bo.orbv = bo.rbv;
    }
    DO_NOT_CONVERT
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let bo = bo(record);

    let val = match format.type_ {
        // the epicsUInt32 arrives zero-extended
        Dbf::Ulong => bo.rval as i64,
        Dbf::Long => {
            if bo.mask != 0 {
                bo.rval as i32 as i64
            } else {
                bo.val as i16 as i64
            }
        }
        Dbf::Enum => bo.val as i64,
        Dbf::String => {
            let name = if bo.val != 0 { &bo.onam } else { &bo.znam };
            return io.printf_string(format, name.as_bytes());
        }
        _ => return ERROR,
    };
    io.printf_long(format, val)
}

struct Bo;

impl StreamRecordType for Bo {
    /// C returns `streamInitRecord`'s status as it is.
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
