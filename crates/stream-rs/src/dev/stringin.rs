//! `devstringinStream.c`: the StreamDevice interface for stringin records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::stringin::StringinRecord;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(stringin,INST_IO,devstringinStream,"stream")`: five entries,
/// `report` NULL (`devStream.h:110`).
pub static DEV_STRINGIN_STREAM: DevSup = DevSup {
    name: "devstringinStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// `sizeof(si->val)`, `MAX_STRING_SIZE`.
const VAL_SIZE: usize = 40;

/// A `DTYP "stream"` stringin's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Stringin)
}

fn stringin(record: &mut dyn Record) -> &mut StringinRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devstringinStream is bound to stringin records only")
}

/// VAL as C's `char[40]`.
fn val_buffer(si: &StringinRecord) -> [u8; VAL_SIZE] {
    let mut buffer = [0u8; VAL_SIZE];
    let val = si.val.as_bytes();
    let n = val.len().min(VAL_SIZE - 1);
    buffer[..n].copy_from_slice(&val[..n]);
    buffer
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let si = stringin(record);

    if format.type_ == Dbf::String {
        // C scans into VAL itself, so a failed scan keeps what it wrote.
        let mut val = val_buffer(si);
        let status = io.scanf_string(format, &mut val);
        let len = val.iter().position(|&b| b == 0).unwrap_or(VAL_SIZE);
        si.val = val[..len].into();
        if status == ERROR as isize {
            return ERROR;
        }
        return OK;
    }
    ERROR
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let si = stringin(record);

    if format.type_ == Dbf::String {
        return io.printf_string(format, si.val.as_bytes());
    }
    ERROR
}

struct Stringin;

impl StreamRecordType for Stringin {
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
