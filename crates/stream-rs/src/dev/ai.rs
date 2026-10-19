//! `devaiStream.c`: the StreamDevice interface for ai records.

use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::c_cast::{f64_to_c_long, f64_to_u64};

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo,
    StreamRecordType,
};

/// `device(ai,INST_IO,devaiStream,"stream")`: six entries, `report`
/// (`streamReport` is NULL) and `special_linconv` NULL.
pub static DEV_AI_STREAM: DevSup = DevSup {
    name: "devaiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` ai's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Ai)
}

fn ai(record: &mut dyn Record) -> &mut AiRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devaiStream is bound to ai records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let ai = ai(record);
    let mut val;

    match format.type_ {
        Dbf::Double => {
            val = 0.0;
            if io.scanf_double(format, &mut val) == ERROR {
                return ERROR;
            }
        }
        Dbf::Ulong | Dbf::Long => {
            let mut rval = 0;
            if io.scanf_long(format, &mut rval) == ERROR {
                return ERROR;
            }
            ai.rval = rval as i32;
            if ai.linr != 0 {
                return OK;
            }
            // allow integers with more than 32 bits
            val = if format.type_ == Dbf::Ulong {
                rval as u64 as f64
            } else {
                rval as f64
            };
        }
        _ => return ERROR,
    }
    if ai.aslo != 0.0 && ai.aslo != 1.0 {
        val *= ai.aslo;
    }
    val += ai.aoff;
    if !(ai.smoo == 0.0
        || ai.init.is_initial()
        || common.udf != 0
        || ai.val.is_infinite()
        || ai.val.is_nan())
    {
        val = ai.val * ai.smoo + val * (1.0 - ai.smoo);
    }
    ai.val = val;
    DO_NOT_CONVERT
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let ai = ai(record);

    let mut val = ai.val - ai.aoff;
    if ai.aslo != 0.0 && ai.aslo != 1.0 {
        val /= ai.aslo;
    }

    match format.type_ {
        Dbf::Double => io.printf_double(format, val),
        Dbf::Ulong => {
            if ai.linr == 0 {
                // allow more bits than 32
                return io.printf_long(format, f64_to_u64(val) as i64);
            }
            // (unsigned long) of an epicsInt32 sign-extends: the long's bits
            io.printf_long(format, ai.rval as i64)
        }
        Dbf::Long => {
            if ai.linr == 0 {
                // allow more bits than 32
                return io.printf_long(format, f64_to_c_long(val));
            }
            io.printf_long(format, ai.rval as i64)
        }
        _ => ERROR,
    }
}

struct Ai;

impl StreamRecordType for Ai {
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
