//! `devscalcoutStream.c`: the StreamDevice interface for scalcout records.

use epics_base_rs::calc::engine::value::SCALC_STRING_SIZE;
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::PvString;
use epics_base_rs::types::c_cast::f64_to_c_long;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(scalcout,INST_IO,devscalcoutStream,"stream")` of
/// `stream-scalcout.dbd`: five entries, `report` (`streamReport` is NULL).
pub static DEV_SCALCOUT_STREAM: DevSup = DevSup {
    name: "devscalcoutStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` scalcout's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Scalcout)
}

fn scalcout(record: &mut dyn Record) -> &mut ScalcoutRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devscalcoutStream is bound to scalcout records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let sco = scalcout(record);

    match format.type_ {
        Dbf::Double => io.scanf_double(format, &mut sco.val),
        Dbf::Long | Dbf::Ulong | Dbf::Enum => {
            let mut lval = 0;
            if io.scanf_long(format, &mut lval) == ERROR {
                return ERROR;
            }
            sco.val = lval as f64;
            OK
        }
        Dbf::String => {
            let mut sval = [0u8; SCALC_STRING_SIZE];
            if io.scanf_string(format, &mut sval) == ERROR as isize {
                return ERROR;
            }
            let end = sval.iter().position(|&b| b == 0).unwrap_or(sval.len());
            sco.sval = PvString::from_bytes(&sval[..end]);
            OK
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
    let sco = scalcout(record);

    match format.type_ {
        Dbf::Double => io.printf_double(format, sco.oval),
        // `(long)sco->oval`, x86's cast (devscalcoutStream.c:81)
        Dbf::Long | Dbf::Ulong | Dbf::Enum => io.printf_long(format, f64_to_c_long(sco.oval)),
        Dbf::String => io.printf_string(format, sco.osv.as_bytes()),
        _ => ERROR,
    }
}

struct Scalcout;

impl StreamRecordType for Scalcout {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}
