//! `devmbbiDirectStream.c`: the StreamDevice interface for mbbiDirect
//! records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo,
    StreamRecordType,
};

/// `device(mbbiDirect,INST_IO,devmbbiDirectStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_MBBI_DIRECT_STREAM: DevSup = DevSup {
    name: "devmbbiDirectStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` mbbiDirect's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(MbbiDirect)
}

fn mbbi_direct(record: &mut dyn Record) -> &mut MbbiDirectRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devmbbiDirectStream is bound to mbbiDirect records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbi_d = mbbi_direct(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long => {
            let mut v = 0;
            if io.scanf_long(format, &mut v) == ERROR {
                return ERROR;
            }
            let mut val = v as u64;
            if mbbi_d.mask != 0 {
                val &= mbbi_d.mask as u64;
                mbbi_d.rval = val as u32;
                return OK;
            }
            // No MASK, (NOBT = 0): use VAL field
            mbbi_d.val = val as u32;
            DO_NOT_CONVERT
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
    let mbbi_d = mbbi_direct(record);

    match format.type_ {
        Dbf::Long | Dbf::Ulong => {
            // VAL is DBF_LONG: C's `val = mbbiD->val` sign-extends it.
            let val = if mbbi_d.mask != 0 {
                (mbbi_d.rval & mbbi_d.mask) as i64
            } else {
                mbbi_d.val as i32 as i64
            };
            io.printf_long(format, val)
        }
        _ => ERROR,
    }
}

struct MbbiDirect;

impl StreamRecordType for MbbiDirect {
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        let m = mbbi_direct(record);
        // C's `mbbiD->mask <<= mbbiD->shft` is undefined for a SHFT of 32 or
        // more; 0 is the fully shifted-out mask, as the record's RVAL shift.
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
