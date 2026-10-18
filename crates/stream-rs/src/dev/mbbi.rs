//! `devmbbiStream.c`: the StreamDevice interface for mbbi records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::mbbi::MbbiRecord;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo,
    StreamRecordType,
};

/// `device(mbbi,INST_IO,devmbbiStream,"stream")`: five entries, `report`
/// (`streamReport` is NULL).
pub static DEV_MBBI_STREAM: DevSup = DevSup {
    name: "devmbbiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` mbbi's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Mbbi)
}

fn mbbi(record: &mut dyn Record) -> &mut MbbiRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devmbbiStream is bound to mbbi records only")
}

/// `if (mbbi->sdef) for (i=0; i<16; i++) if ((&mbbi->zrvl)[i])`
fn raw_values_defined(mbbi: &MbbiRecord) -> bool {
    mbbi.sdef() && mbbi.raw_values().iter().any(|&v| v != 0)
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbi = mbbi(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long => {
            let mut v = 0;
            if io.scanf_long(format, &mut v) == ERROR {
                return ERROR;
            }
            let mut val = v as u64;
            // read VAL or RBV? Look if any value is defined
            if raw_values_defined(mbbi) {
                if mbbi.mask != 0 {
                    val &= mbbi.mask as u64;
                }
                mbbi.rval = val as u16 as u32;
                return OK;
            }
            mbbi.val = val as u16;
            DO_NOT_CONVERT
        }
        Dbf::Enum => {
            let mut v = 0;
            if io.scanf_long(format, &mut v) == ERROR {
                return ERROR;
            }
            mbbi.val = v as u16;
            DO_NOT_CONVERT
        }
        Dbf::String => {
            // sizeof(mbbi->zrst)
            let mut buffer = [0u8; 26];
            if io.scanf_string(format, &mut buffer) == ERROR as isize {
                return ERROR;
            }
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            match mbbi
                .state_strings()
                .iter()
                .position(|s| s.as_bytes() == &buffer[..end])
            {
                Some(i) => {
                    mbbi.val = i as u16;
                    DO_NOT_CONVERT
                }
                None => ERROR,
            }
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
    let mbbi = mbbi(record);

    match format.type_ {
        Dbf::Long | Dbf::Ulong => {
            // print VAL or RVAL ? Look if any value is defined
            let mut val = mbbi.val as i64;
            if raw_values_defined(mbbi) {
                val = mbbi.rval as i64;
                if mbbi.mask != 0 {
                    val &= mbbi.mask as i64;
                }
            }
            io.printf_long(format, val)
        }
        Dbf::Enum => io.printf_long(format, mbbi.val as i64),
        Dbf::String => {
            if mbbi.val >= 16 {
                return ERROR;
            }
            io.printf_string(format, mbbi.state_strings()[mbbi.val as usize].as_bytes())
        }
        _ => ERROR,
    }
}

struct Mbbi;

impl StreamRecordType for Mbbi {
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        let m = mbbi(record);
        // C's `mbbi->mask <<= mbbi->shft` is undefined for a SHFT of 32 or
        // more; 0 is the fully shifted-out mask, as MbbiRecord's RVAL shift.
        m.mask = m.mask.checked_shl(m.shft as u32).unwrap_or(0);
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
