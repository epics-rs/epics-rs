//! `devlongoutStream.c`: the StreamDevice interface for longout records.

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::types::EpicsValue;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(longout,INST_IO,devlongoutStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_LONGOUT_STREAM: DevSup = DevSup {
    name: "devlongoutStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` longout's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Longout)
}

fn longout(record: &mut dyn Record) -> &mut LongoutRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devlongoutStream is bound to longout records only")
}

/// `DELTA(last, val) > del`. devlongoutStream.c:27 subtracts in `int` and
/// overflows once the two are more than INT_MAX apart; the magnitude is
/// taken in i64, as longoutRecord.c:367's unsigned DELTA does.
fn exceeds(last: i32, val: i32, del: i32) -> bool {
    (last as i64 - val as i64).abs() > del as i64
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lo = longout(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut val = 0;
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            lo.val = val as i32;
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
    if exceeds(lo.mlst, lo.val, lo.mdel) {
        monitor_mask |= EventMask::VALUE;
        lo.mlst = lo.val;
    }
    if exceeds(lo.alst, lo.val, lo.adel) {
        monitor_mask |= EventMask::LOG;
        lo.alst = lo.val;
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::Long(lo.val), monitor_mask);
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lo = longout(record);

    match format.type_ {
        // devlongoutStream.c:73 passes the epicsInt32 uncast where
        // Stream::print takes a long: it arrives zero-extended, VAL's u32
        Dbf::Ulong => io.printf_long(format, lo.val as u32 as i64),
        Dbf::Long | Dbf::Enum => io.printf_long(format, lo.val as i64),
        _ => ERROR,
    }
}

struct Longout;

impl StreamRecordType for Longout {
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
