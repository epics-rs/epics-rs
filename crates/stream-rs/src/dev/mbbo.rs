//! `devmbboStream.c`: the StreamDevice interface for mbbo records.

use epics_base_rs::server::recgbl::{
    EventMask, alarm_status, rec_gbl_reset_alarms, rec_gbl_set_sevr_raw,
};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::mbbo::MbboRecord;

use crate::epics::{
    DO_NOT_CONVERT, Dbf, ERROR, Format, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(mbbo,INST_IO,devmbboStream,"stream")`: five entries, `report`
/// (`streamReport` is NULL).
pub static DEV_MBBO_STREAM: DevSup = DevSup {
    name: "devmbboStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false).with_init(crate::epics::stream_init),
};

/// A `DTYP "stream"` mbbo's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Mbbo)
}

fn mbbo(record: &mut dyn Record) -> &mut MbboRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devmbboStream is bound to mbbo records only")
}

/// The record's VAL, RVAL or RBV as it serves it.
fn post(io: &StreamIo<'_>, mbbo: &MbboRecord, field: &'static str, mask: EventMask) {
    if let Some(value) = Record::get_field(mbbo, field) {
        io.post_events(field, value, mask);
    }
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbo = mbbo(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Long | Dbf::Enum => {
            let mut v = 0;
            if io.scanf_long(format, &mut v) == ERROR {
                return ERROR;
            }
            let mut val = v as u64;
            if mbbo.mask != 0 {
                val &= mbbo.mask as u64;
            }
            mbbo.rbv = val as u32;
            mbbo.rval = val as u32;
            if mbbo.shft > 0 {
                val = val.checked_shr(mbbo.shft as u32).unwrap_or(0);
            }
            // read VAL or RBV? Look if any value is defined
            if mbbo.sdef() {
                mbbo.val = mbbo
                    .raw_values()
                    .iter()
                    .position(|&rv| rv as u64 == val)
                    .map_or(65535, |i| i as u16);
            } else {
                mbbo.val = val as u16;
            }
        }
        Dbf::String => {
            // sizeof(mbbo->zrst)
            let mut buffer = [0u8; 26];
            if io.scanf_string(format, &mut buffer) == ERROR as isize {
                return ERROR;
            }
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            mbbo.val = mbbo
                .state_strings()
                .iter()
                .position(|s| s.as_bytes() == &buffer[..end])
                .map_or(65535, |i| i as u16);
        }
        _ => return ERROR,
    }
    if io.pact() {
        return DO_NOT_CONVERT;
    }
    // In @init handler, no processing, enforce monitor updates.
    let reset = rec_gbl_reset_alarms(common);
    io.post_alarm_reset(common, &reset);
    let mut monitor_mask = if reset.alarm_changed || reset.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };
    let sevr = if mbbo.val > 15 {
        mbbo.unsv
    } else {
        mbbo.state_severities()[mbbo.val as usize]
    };
    rec_gbl_set_sevr_raw(common, alarm_status::STATE_ALARM, sevr as u16);
    // devmbboStream.c:88 sets LALM before :89 compares it with VAL, so C
    // never raises COS. LALM advances even when COS is raised, as in
    // MbboRecord::check_alarms, so a rerun @init does not re-raise it.
    if mbbo.val != mbbo.lalm {
        rec_gbl_set_sevr_raw(common, alarm_status::COS_ALARM, mbbo.cosv as u16);
        mbbo.lalm = mbbo.val;
    }
    if mbbo.mlst != mbbo.val {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
        mbbo.mlst = mbbo.val;
    }
    if !monitor_mask.is_empty() {
        post(io, mbbo, "VAL", monitor_mask);
    }
    if mbbo.oraw != mbbo.rval {
        post(io, mbbo, "RVAL", monitor_mask | EventMask::VALUE);
        mbbo.oraw = mbbo.rval;
    }
    if mbbo.orbv != mbbo.rbv {
        post(io, mbbo, "RBV", monitor_mask | EventMask::VALUE);
        mbbo.orbv = mbbo.rbv;
    }
    DO_NOT_CONVERT
}

/// `val <<= mbbo->shft`, then RVAL when any of ZRVL..FFVL is defined, then
/// `val &= mbbo->mask`; `val` and `rval` are VAL and RVAL as `long`.
fn raw_out(mbbo: &MbboRecord, val: i64, rval: i64) -> i64 {
    let mut val = val;
    if mbbo.shft > 0 {
        // C's shift is undefined from 64 on; 0 is the fully shifted-out value.
        val = val.checked_shl(mbbo.shft as u32).unwrap_or(0);
    }
    // print VAL or RVAL ? any values defined ?
    if mbbo.sdef() && mbbo.raw_values().iter().any(|&v| v != 0) {
        val = rval;
    }
    if mbbo.mask != 0 {
        val &= mbbo.mask as i64;
    }
    val
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mbbo = mbbo(record);

    match format.type_ {
        Dbf::Ulong | Dbf::Enum => {
            io.printf_long(format, raw_out(mbbo, mbbo.val as i64, mbbo.rval as i64))
        }
        Dbf::Long => io.printf_long(
            format,
            raw_out(mbbo, mbbo.val as i16 as i64, mbbo.rval as i32 as i64),
        ),
        Dbf::String => {
            if mbbo.val >= 16 {
                return ERROR;
            }
            io.printf_string(format, mbbo.state_strings()[mbbo.val as usize].as_bytes())
        }
        _ => ERROR,
    }
}

struct Mbbo;

impl StreamRecordType for Mbbo {
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        let m = mbbo(record);
        // C's `mbbo->mask <<= mbbo->shft` is undefined for a SHFT of 32 or
        // more; 0 is the fully shifted-out mask, as MbboRecord's RVAL shift.
        m.mask = m.mask.checked_shl(m.shft as u32).unwrap_or(0);
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            // devmbboStream.c:167 returns streamInitRecord's status unmapped.
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}
