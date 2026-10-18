//! `devstringoutStream.c`: the StreamDevice interface for stringout records.

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::{EpicsValue, PvString};

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(stringout,INST_IO,devstringoutStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_STRINGOUT_STREAM: DevSup = DevSup {
    name: "devstringoutStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// `sizeof(so->val)`, `MAX_STRING_SIZE`.
const VAL_SIZE: usize = 40;

/// `stringoutPOST_Always`
const POST_ALWAYS: i16 = 1;

/// A `DTYP "stream"` stringout's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Stringout)
}

fn stringout(record: &mut dyn Record) -> &mut StringoutRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devstringoutStream is bound to stringout records only")
}

/// VAL as C's `char[40]`.
fn val_buffer(val: &PvString) -> [u8; VAL_SIZE] {
    let mut buffer = [0u8; VAL_SIZE];
    let val = val.as_bytes();
    let n = val.len().min(VAL_SIZE - 1);
    buffer[..n].copy_from_slice(&val[..n]);
    buffer
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let so = stringout(record);

    if format.type_ != Dbf::String {
        return ERROR;
    }
    // C scans into VAL itself, so a failed scan keeps what it wrote.
    let mut val = val_buffer(&so.val);
    let status = io.scanf_string(format, &mut val);
    let len = val.iter().position(|&b| b == 0).unwrap_or(VAL_SIZE);
    so.val = val[..len].into();
    if status == ERROR as isize {
        return ERROR;
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
    if so.mpst == POST_ALWAYS {
        monitor_mask |= EventMask::VALUE;
    }
    if so.apst == POST_ALWAYS {
        monitor_mask |= EventMask::LOG;
    }
    // C skips the compare once the mask is DBE_VALUE|DBE_LOG
    // (devstringoutStream.c:39-40) and so leaves OVAL behind VAL;
    // stringoutRecord.c's monitor() and devlsoStream.c:46-49 always copy.
    if so.oval != so.val {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
        so.oval.clone_from(&so.val);
    }
    if !monitor_mask.is_empty() {
        io.post_events("VAL", EpicsValue::String(so.val.clone()), monitor_mask);
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let so = stringout(record);

    if format.type_ == Dbf::String {
        return io.printf_string(format, so.val.as_bytes());
    }
    ERROR
}

struct Stringout;

impl StreamRecordType for Stringout {
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
