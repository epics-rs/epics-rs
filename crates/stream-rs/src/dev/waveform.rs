//! `devwaveformStream.c`: the StreamDevice interface for waveform records.

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::server::record::{
    CommonFields, DbLinkType, DevSup, Dset, Ftype, MENU_FTYPE, Record,
};
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{EpicsValue, PvString};

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `MAX_STRING_SIZE`: an FTVL STRING element's bytes.
const MAX_STRING_SIZE: usize = 40;

/// `device(waveform,INST_IO,devwaveformStream,"stream")`: five entries,
/// `report` (`streamReport` is NULL).
pub static DEV_WAVEFORM_STREAM: DevSup = DevSup {
    name: "devwaveformStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` waveform's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Waveform)
}

fn waveform(record: &mut dyn Record) -> &mut WaveformRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devwaveformStream is bound to waveform records only")
}

/// `pamapdbfType[ftvl].strvalue`
fn dbf_name(ftvl: Ftype) -> String {
    format!("DBF_{}", MENU_FTYPE[ftvl.index() as usize])
}

/// C's `(long)`: 32 bits where `long` is
fn c_long(v: i64) -> i64 {
    if size_of::<std::ffi::c_long>() == 4 {
        v as i32 as i64
    } else {
        v
    }
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let wf = waveform(record);
    let ftvl = wf.ftvl();

    wf.rarm = 0;
    wf.nord = 0;
    while wf.nord < wf.nelm {
        let nord = wf.nord as usize;
        match format.type_ {
            Dbf::Double => {
                let mut dval = 0.0;
                if io.scanf_double(format, &mut dval) == ERROR {
                    return if wf.nord != 0 { OK } else { ERROR };
                }
                match &mut wf.val {
                    EpicsValue::DoubleArray(a) => a.make_mut()[nord] = dval,
                    EpicsValue::FloatArray(a) => a.make_mut()[nord] = dval as f32,
                    _ => {
                        errlog_sev_vprintf(
                            ErrlogSevEnum::Fatal,
                            format_args!(
                                "readData {}: can't convert from double to {}\n",
                                io.name(),
                                dbf_name(ftvl)
                            ),
                        );
                        return ERROR;
                    }
                }
            }
            Dbf::Ulong | Dbf::Long | Dbf::Enum => {
                let mut lval = 0;
                if io.scanf_long(format, &mut lval) == ERROR {
                    return if wf.nord != 0 { OK } else { ERROR };
                }
                match &mut wf.val {
                    EpicsValue::DoubleArray(a) => a.make_mut()[nord] = lval as f64,
                    EpicsValue::FloatArray(a) => a.make_mut()[nord] = lval as f32,
                    EpicsValue::Int64Array(a) => a.make_mut()[nord] = lval,
                    EpicsValue::UInt64Array(a) => a.make_mut()[nord] = lval as u64,
                    EpicsValue::LongArray(a) => a.make_mut()[nord] = lval as i32,
                    EpicsValue::ULongArray(a) => a.make_mut()[nord] = lval as u32,
                    EpicsValue::ShortArray(a) => a.make_mut()[nord] = lval as i16,
                    EpicsValue::UShortArray(a) => a.make_mut()[nord] = lval as u16,
                    EpicsValue::EnumArray(a) => a.make_mut()[nord] = lval as u16,
                    EpicsValue::CharArray(a) => a.make_mut()[nord] = lval as u8,
                    EpicsValue::UCharArray(a) => a.make_mut()[nord] = lval as u8,
                    _ => {
                        errlog_sev_vprintf(
                            ErrlogSevEnum::Fatal,
                            format_args!(
                                "readData {}: can't convert from long to {}\n",
                                io.name(),
                                dbf_name(ftvl)
                            ),
                        );
                        return ERROR;
                    }
                }
            }
            Dbf::String => match &mut wf.val {
                EpicsValue::StringArray(a) => {
                    let element = &mut a.make_mut()[nord];
                    let mut buffer = [0u8; MAX_STRING_SIZE];
                    let old = element.as_bytes();
                    let n = old.len().min(MAX_STRING_SIZE);
                    buffer[..n].copy_from_slice(&old[..n]);
                    if io.scanf_string(format, &mut buffer) == ERROR as isize {
                        return if wf.nord != 0 { OK } else { ERROR };
                    }
                    let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
                    *element = PvString::from_bytes(&buffer[..len]);
                }
                EpicsValue::CharArray(a) | EpicsValue::UCharArray(a) => {
                    let nelm = wf.nelm as usize;
                    wf.nord = 0;
                    let buffer = &mut a.make_mut()[..nelm];
                    let length = io.scanf_string(format, buffer);
                    if length == ERROR as isize {
                        return ERROR;
                    }
                    if (length as usize) < nelm {
                        buffer[length as usize] = 0;
                    }
                    wf.nord = length as i32;
                    return OK;
                }
                _ => {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Fatal,
                        format_args!(
                            "readData {}: can't convert from string to {}\n",
                            io.name(),
                            dbf_name(ftvl)
                        ),
                    );
                    return ERROR;
                }
            },
            Dbf::Char => {
                errlog_sev_vprintf(
                    ErrlogSevEnum::Major,
                    format_args!(
                        "readData {}: can't convert from {} to {}\n",
                        io.name(),
                        format.type_.name(),
                        dbf_name(ftvl)
                    ),
                );
                return ERROR;
            }
        }
        wf.nord += 1;
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let wf = waveform(record);
    let ftvl = wf.ftvl();

    for nowd in 0..wf.nord.max(0) as usize {
        match format.type_ {
            Dbf::Double => {
                let dval = match &wf.val {
                    EpicsValue::DoubleArray(a) => a.as_slice()[nowd],
                    EpicsValue::FloatArray(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::Int64Array(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::UInt64Array(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::LongArray(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::ULongArray(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::ShortArray(a) => a.as_slice()[nowd] as f64,
                    // C reads an ENUM element as epicsInt16
                    EpicsValue::EnumArray(a) => a.as_slice()[nowd] as i16 as f64,
                    EpicsValue::UShortArray(a) => a.as_slice()[nowd] as f64,
                    EpicsValue::CharArray(a) => a.as_slice()[nowd] as i8 as f64,
                    EpicsValue::UCharArray(a) => a.as_slice()[nowd] as f64,
                    _ => {
                        errlog_sev_vprintf(
                            ErrlogSevEnum::Fatal,
                            format_args!(
                                "writeData {}: can't convert from {} to double\n",
                                io.name(),
                                dbf_name(ftvl)
                            ),
                        );
                        return ERROR;
                    }
                };
                if io.printf_double(format, dval) != OK {
                    return ERROR;
                }
            }
            Dbf::Long | Dbf::Ulong | Dbf::Enum => {
                let lval = match &wf.val {
                    EpicsValue::Int64Array(a) => c_long(a.as_slice()[nowd]),
                    EpicsValue::UInt64Array(a) => c_long(a.as_slice()[nowd] as i64),
                    EpicsValue::LongArray(a) => a.as_slice()[nowd] as i64,
                    EpicsValue::ULongArray(a) => a.as_slice()[nowd] as i64,
                    EpicsValue::ShortArray(a) => a.as_slice()[nowd] as i64,
                    EpicsValue::EnumArray(a) => a.as_slice()[nowd] as i16 as i64,
                    EpicsValue::UShortArray(a) => a.as_slice()[nowd] as i64,
                    EpicsValue::CharArray(a) => a.as_slice()[nowd] as i8 as i64,
                    EpicsValue::UCharArray(a) => a.as_slice()[nowd] as i64,
                    _ => {
                        errlog_sev_vprintf(
                            ErrlogSevEnum::Fatal,
                            format_args!(
                                "writeData {}: can't convert from {} to long\n",
                                io.name(),
                                dbf_name(ftvl)
                            ),
                        );
                        return ERROR;
                    }
                };
                if io.printf_long(format, lval) != OK {
                    return ERROR;
                }
            }
            Dbf::String => match &mut wf.val {
                EpicsValue::StringArray(a) => {
                    if io.printf_string(format, a.as_slice()[nowd].as_bytes()) != OK {
                        return ERROR;
                    }
                }
                EpicsValue::CharArray(a) | EpicsValue::UCharArray(a) => {
                    // print waveform as a null-terminated string
                    let nelm = wf.nelm as usize;
                    let buffer = a.make_mut();
                    if (wf.nord as usize) < nelm {
                        buffer[wf.nord as usize] = 0;
                    } else {
                        // as C: a full VAL loses its last byte to the terminator
                        buffer[nelm - 1] = 0;
                    }
                    if io.printf_string(format, buffer) != OK {
                        return ERROR;
                    }
                    return OK;
                }
                _ => {
                    errlog_sev_vprintf(
                        ErrlogSevEnum::Fatal,
                        format_args!(
                            "writeData {}: can't convert from {} to string\n",
                            io.name(),
                            dbf_name(ftvl)
                        ),
                    );
                    return ERROR;
                }
            },
            Dbf::Char => {
                errlog_sev_vprintf(
                    ErrlogSevEnum::Fatal,
                    format_args!(
                        "writeData {}: can't convert from {} to {}\n",
                        io.name(),
                        dbf_name(ftvl),
                        format.type_.name()
                    ),
                );
                return ERROR;
            }
        }
    }
    OK
}

struct Waveform;

impl StreamRecordType for Waveform {
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
