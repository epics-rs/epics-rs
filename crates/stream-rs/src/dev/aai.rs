//! `devaaiStream.c`: the StreamDevice interface for aai records.

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_vprintf};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Ftype, Record};
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{EpicsValue, PvString};

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// dbDefs.h `MAX_STRING_SIZE`: a DBF_STRING element's bytes.
const MAX_STRING_SIZE: usize = 40;

/// `device(aai,INST_IO,devaaiStream,"stream")`: five entries, `report`
/// (`streamReport` is NULL).
pub static DEV_AAI_STREAM: DevSup = DevSup {
    name: "devaaiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// A `DTYP "stream"` aai's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Aai)
}

fn aai(record: &mut dyn Record) -> &mut WaveformRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devaaiStream is bound to aai records only")
}

/// `pamapdbfType[aai->ftvl].strvalue`: menuFtype's index is the DBF code.
fn ftvl_name(ftvl: Ftype) -> String {
    format!("DBF_{}", ftvl.element_type().dbf_code().name())
}

fn fatal(args: std::fmt::Arguments<'_>) -> i64 {
    errlog_sev_vprintf(ErrlogSevEnum::Fatal, args);
    ERROR
}

/// `((type *)aai->bptr)[i] = value` for the long and double arms, or
/// `None` where readData's switch has no case for FTVL.
fn store(val: &mut EpicsValue, nelm: usize, i: usize, value: Number) -> Option<()> {
    use EpicsValue::*;
    match (val, value) {
        (DoubleArray(v), Number::Double(d)) => v.make_mut_resized(nelm, 0.0)[i] = d,
        (FloatArray(v), Number::Double(d)) => v.make_mut_resized(nelm, 0.0)[i] = d as f32,
        (DoubleArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0.0)[i] = l as f64,
        (FloatArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0.0)[i] = l as f32,
        (Int64Array(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l,
        (UInt64Array(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l as u64,
        (LongArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l as i32,
        (ULongArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l as u32,
        (ShortArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l as i16,
        (UShortArray(v) | EnumArray(v), Number::Long(l)) => {
            v.make_mut_resized(nelm, 0)[i] = l as u16
        }
        (CharArray(v) | UCharArray(v), Number::Long(l)) => v.make_mut_resized(nelm, 0)[i] = l as u8,
        _ => return None,
    }
    Some(())
}

#[derive(Clone, Copy)]
enum Number {
    Double(f64),
    Long(i64),
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let aai = aai(record);
    let nelm = aai.nelm.max(0) as usize;
    let ftvl = aai.ftvl();
    let name = io.name();

    aai.nord = 0;
    while (aai.nord as usize) < nelm {
        let nord = aai.nord as usize;
        match format.type_ {
            Dbf::Double => {
                let mut dval = 0.0;
                if io.scanf_double(format, &mut dval) == ERROR {
                    return if nord != 0 { OK } else { ERROR };
                }
                if !matches!(ftvl, Ftype::Double | Ftype::Float) {
                    return fatal(format_args!(
                        "readData {name}: can't convert from double to {}\n",
                        ftvl_name(ftvl)
                    ));
                }
                store(&mut aai.val, nelm, nord, Number::Double(dval));
            }
            Dbf::Ulong | Dbf::Long | Dbf::Enum => {
                let mut lval = 0;
                if io.scanf_long(format, &mut lval) == ERROR {
                    return if nord != 0 { OK } else { ERROR };
                }
                if store(&mut aai.val, nelm, nord, Number::Long(lval)).is_none() {
                    return fatal(format_args!(
                        "readData {name}: can't convert from long to {}\n",
                        ftvl_name(ftvl)
                    ));
                }
            }
            Dbf::String => match &mut aai.val {
                EpicsValue::StringArray(v) => {
                    let mut buf = [0u8; MAX_STRING_SIZE];
                    if io.scanf_string(format, &mut buf) == ERROR as isize {
                        return if nord != 0 { OK } else { ERROR };
                    }
                    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    v.make_mut_resized(nelm, PvString::new())[nord] =
                        PvString::from_bytes(&buf[..len]);
                }
                EpicsValue::CharArray(v) | EpicsValue::UCharArray(v) => {
                    aai.nord = 0;
                    let bptr = v.make_mut_resized(nelm, 0);
                    let length = io.scanf_string(format, bptr);
                    if length == ERROR as isize {
                        return ERROR;
                    }
                    if (length as usize) < nelm {
                        bptr[length as usize] = 0;
                    }
                    aai.nord = length as i32;
                    return OK;
                }
                _ => {
                    return fatal(format_args!(
                        "readData {name}: can't convert from string to {}\n",
                        ftvl_name(ftvl)
                    ));
                }
            },
            _ => {
                errlog_sev_vprintf(
                    ErrlogSevEnum::Major,
                    format_args!(
                        "readData {name}: can't convert from {} to {}\n",
                        format.type_.name(),
                        ftvl_name(ftvl)
                    ),
                );
                return ERROR;
            }
        }
        aai.nord += 1;
    }
    OK
}

/// `((type *)aai->bptr)[i]` as writeData's double arm reads it, or `None`
/// where its switch has no case for FTVL.
fn load_double(val: &EpicsValue, i: usize) -> Option<f64> {
    use EpicsValue::*;
    Some(match val {
        DoubleArray(v) => v.buffer()[i],
        FloatArray(v) => v.buffer()[i] as f64,
        Int64Array(v) => v.buffer()[i] as f64,
        UInt64Array(v) => v.buffer()[i] as f64,
        _ => load_long(val, i)? as f64,
    })
}

/// writeData's long arm: the integer FTVLs. ENUM is read as epicsInt16.
fn load_long(val: &EpicsValue, i: usize) -> Option<i64> {
    use EpicsValue::*;
    Some(match val {
        Int64Array(v) => v.buffer()[i],
        UInt64Array(v) => v.buffer()[i] as i64,
        LongArray(v) => v.buffer()[i] as i64,
        ULongArray(v) => v.buffer()[i] as i64,
        ShortArray(v) => v.buffer()[i] as i64,
        EnumArray(v) => v.buffer()[i] as i16 as i64,
        UShortArray(v) => v.buffer()[i] as i64,
        CharArray(v) => v.buffer()[i] as i8 as i64,
        UCharArray(v) => v.buffer()[i] as i64,
        _ => return None,
    })
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let aai = aai(record);
    let nelm = aai.nelm.max(0) as usize;
    let nord = aai.nord.max(0) as usize;
    let ftvl = aai.ftvl();
    let name = io.name();

    for nowd in 0..nord {
        match format.type_ {
            Dbf::Double => {
                let Some(dval) = load_double(&aai.val, nowd) else {
                    return fatal(format_args!(
                        "writeData {name}: can't convert from {} to double\n",
                        ftvl_name(ftvl)
                    ));
                };
                if io.printf_double(format, dval) != OK {
                    return ERROR;
                }
            }
            Dbf::Ulong | Dbf::Long | Dbf::Enum => {
                let Some(lval) = load_long(&aai.val, nowd) else {
                    return fatal(format_args!(
                        "writeData {name}: can't convert from {} to long\n",
                        ftvl_name(ftvl)
                    ));
                };
                if io.printf_long(format, lval) != OK {
                    return ERROR;
                }
            }
            Dbf::String => match &mut aai.val {
                EpicsValue::StringArray(v) => {
                    if io.printf_string(format, v.buffer()[nowd].as_bytes()) != OK {
                        return ERROR;
                    }
                }
                EpicsValue::CharArray(v) | EpicsValue::UCharArray(v) => {
                    // print aai as a null-terminated string: C writes the
                    // terminator into VAL, over the last element when full
                    let bptr = v.make_mut_resized(nelm, 0);
                    bptr[if nord < nelm { nord } else { nelm - 1 }] = 0;
                    if io.printf_string(format, bptr) != OK {
                        return ERROR;
                    }
                    return OK;
                }
                _ => {
                    return fatal(format_args!(
                        "writeData {name}: can't convert from {} to string\n",
                        ftvl_name(ftvl)
                    ));
                }
            },
            _ => {
                return fatal(format_args!(
                    "writeData {name}: can't convert from {} to {}\n",
                    ftvl_name(ftvl),
                    format.type_.name()
                ));
            }
        }
    }
    OK
}

struct Aai;

impl StreamRecordType for Aai {
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        // calloc(nelm, dbValueSize(ftvl)), NORD untouched; Rust aborts
        // where C reports "can't allocate memory for data array"
        let aai = aai(record);
        aai.val = aai.ftvl().zeroed(aai.nelm.max(0) as usize);
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
