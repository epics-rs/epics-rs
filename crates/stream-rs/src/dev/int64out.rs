//! `devint64outStream.c`: the StreamDevice interface for int64out records.

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::types::EpicsValue;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(int64out,INST_IO,devint64outStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_INT64OUT_STREAM: DevSup = DevSup {
    name: "devint64outStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` int64out's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Int64out)
}

fn int64out(record: &mut dyn Record) -> &mut Int64outRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devint64outStream is bound to int64out records only")
}

/// `DELTA(last, val) > del`. devint64outStream.c:27 subtracts in
/// epicsInt64 and overflows once the two are more than INT64_MAX apart;
/// the magnitude is taken in i128, as int64outRecord.c:350's unsigned
/// DELTA does.
fn exceeds(last: i64, val: i64, del: i64) -> bool {
    (last as i128 - val as i128).abs() > del as i128
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let i64o = int64out(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut val = 0;
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            // C's `(unsigned long)val` for the other types keeps the bits
            // on a 64-bit long, so both arms store `val`
            i64o.val = val;
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
    if exceeds(i64o.mlst, i64o.val, i64o.mdel) {
        monitor_mask |= EventMask::VALUE;
        i64o.mlst = i64o.val;
    }
    if exceeds(i64o.alst, i64o.val, i64o.adel) {
        monitor_mask |= EventMask::LOG;
        i64o.alst = i64o.val;
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::Int64(i64o.val), monitor_mask);
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let i64o = int64out(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Enum | Dbf::Long => io.printf_long(format, i64o.val),
        _ => ERROR,
    }
}

struct Int64out;

impl StreamRecordType for Int64out {
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
