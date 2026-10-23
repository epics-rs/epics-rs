//! `devlonginStream.c`: the StreamDevice interface for longin records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::longin::LonginRecord;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(longin,INST_IO,devlonginStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL) (devlonginStream.c:68-82).
pub static DEV_LONGIN_STREAM: DevSup = DevSup {
    name: "devlonginStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` longin's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Longin)
}

fn longin(record: &mut dyn Record) -> &mut LonginRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devlonginStream is bound to longin records only")
}

/// `readData` (devlonginStream.c:26-43).
fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let li = longin(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut val = 0;
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            // long to epicsInt32: gcc keeps the low 32 bits
            li.val = val as i32;
            OK
        }
        _ => ERROR,
    }
}

/// `writeData` (devlonginStream.c:45-58).
fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let li = longin(record);

    match format.type_ {
        // (unsigned long) of an epicsInt32 sign-extends: the long's bits
        Dbf::Ulong => io.printf_long(format, li.val as i64),
        Dbf::Long | Dbf::Enum => io.printf_long(format, li.val as i64),
        _ => ERROR,
    }
}

struct Longin;

/// `initRecord` (devlonginStream.c:60-66).
impl StreamRecordType for Longin {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.inp.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            map_error: true,
        }
    }

    fn is_output(&self) -> bool {
        false
    }
}
