//! `devbiStream.c`: the StreamDevice interface for bi records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::bi::BiRecord;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo,
    StreamRecordType,
};

/// `device(bi,INST_IO,devbiStream,"stream")`: five entries, `report` NULL
/// (`devStream.h:110`).
pub static DEV_BI_STREAM: DevSup = DevSup {
    name: "devbiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// `sizeof(bi->znam)`, biRecord.dbd's `size(26)`.
const ZNAM_SIZE: usize = 26;

/// A `DTYP "stream"` bi's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Bi)
}

fn bi(record: &mut dyn Record) -> &mut BiRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devbiStream is bound to bi records only")
}

/// A `char[]` up to its first 0x00, as `strcmp` reads it.
fn c_str(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let bi = bi(record);
    let mut val = 0;

    match format.type_ {
        Dbf::Ulong | Dbf::Long => {
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            if bi.mask != 0 {
                val &= bi.mask as i64;
            }
            bi.rval = val as u32;
            OK
        }
        Dbf::Enum => {
            if io.scanf_long(format, &mut val) == ERROR {
                return ERROR;
            }
            bi.val = (val != 0) as u16;
            DO_NOT_CONVERT
        }
        Dbf::String => {
            let mut buffer = [0u8; ZNAM_SIZE];
            if io.scanf_string(format, &mut buffer) == ERROR as isize {
                return ERROR;
            }
            let buffer = c_str(&buffer);
            if c_str(bi.znam.as_bytes()) == buffer {
                bi.val = 0;
                return DO_NOT_CONVERT;
            }
            if c_str(bi.onam.as_bytes()) == buffer {
                bi.val = 1;
                return DO_NOT_CONVERT;
            }
            ERROR
        }
        _ => ERROR,
    }
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let bi = bi(record);

    match format.type_ {
        // The epicsUInt32 passed where streamPrintf reads a long arrives
        // zero-extended.
        Dbf::Ulong | Dbf::Long => io.printf_long(format, bi.rval as i64),
        Dbf::Enum => io.printf_long(format, bi.val as i64),
        Dbf::String => io.printf_string(
            format,
            if bi.val != 0 {
                bi.onam.as_bytes()
            } else {
                bi.znam.as_bytes()
            },
        ),
        _ => ERROR,
    }
}

struct Bi;

impl StreamRecordType for Bi {
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
