//! `devint64inStream.c`: the StreamDevice interface for int64in records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::int64in::Int64inRecord;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(int64in,INST_IO,devint64inStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL) (devint64inStream.c:70-84).
pub static DEV_INT64IN_STREAM: DevSup = DevSup {
    name: "devint64inStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` int64in's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Int64in)
}

fn int64in(record: &mut dyn Record) -> &mut Int64inRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devint64inStream is bound to int64in records only")
}

/// `readData` (devint64inStream.c:26-46).
fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let i64i = int64in(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut val = 0;
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            // C's `(unsigned long)val` for the other types keeps the bits
            // on a 64-bit long, so both arms store `val`
            i64i.val = val;
            OK
        }
        _ => ERROR,
    }
}

/// `writeData` (devint64inStream.c:48-60).
fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let i64i = int64in(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Enum | Dbf::Long => io.printf_long(format, i64i.val),
        _ => ERROR,
    }
}

struct Int64in;

/// `initRecord` (devint64inStream.c:62-68).
impl StreamRecordType for Int64in {
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
