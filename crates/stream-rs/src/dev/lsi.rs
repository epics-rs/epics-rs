//! `devlsiStream.c`: the StreamDevice interface for lsi records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::lsi::LsiRecord;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(lsi,INST_IO,devlsiStream,"stream")`: five entries, `report` NULL
/// (`devStream.h:110`).
pub static DEV_LSI_STREAM: DevSup = DevSup {
    name: "devlsiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` lsi's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Lsi)
}

fn lsi(record: &mut dyn Record) -> &mut LsiRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devlsiStream is bound to lsi records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lsi = lsi(record);

    if format.type_ != Dbf::String {
        return ERROR;
    }
    let sizv = lsi.sizv as usize;
    let read = lsi.val.scan_in_place(sizv, |val| {
        let length = io.scanf_string(format, val);
        (length != ERROR as isize).then_some(length as usize)
    });
    if read.is_none() {
        return ERROR;
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lsi = lsi(record);

    if format.type_ != Dbf::String {
        return ERROR;
    }
    // VAL up to its NUL, past LEN where a failed read wrote.
    io.printf_string(format, lsi.val.elements().buffer())
}

struct Lsi;

impl StreamRecordType for Lsi {
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
