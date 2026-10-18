//! `devlsoStream.c`: the StreamDevice interface for lso records.

use epics_base_rs::server::recgbl::{EventMask, rec_gbl_reset_alarms};
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::lso::LsoRecord;
use epics_base_rs::types::EpicsValue;

use crate::epics::{
    Dbf, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

/// `device(lso,INST_IO,devlsoStream,"stream")`: five entries, `report`
/// (`streamReport` is NULL).
pub static DEV_LSO_STREAM: DevSup = DevSup {
    name: "devlsoStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[1, 2, 3, 4], false),
};

/// `menuPost_Always`
const POST_ALWAYS: i16 = 1;

/// A `DTYP "stream"` lso's device.
pub fn device() -> StreamDevice {
    StreamDevice::new(Lso)
}

fn lso(record: &mut dyn Record) -> &mut LsoRecord {
    record
        .as_any_mut()
        .and_then(|a| a.downcast_mut())
        .expect("devlsoStream is bound to lso records only")
}

fn read_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    common: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lso = lso(record);

    if format.type_ != Dbf::String {
        return ERROR;
    }
    // C scans into VAL's SIZV bytes itself, LEN elements of them shown, so
    // a failed scan keeps what it wrote under the old LEN.
    let sizv = lso.sizv as usize;
    let mut val = vec![0u8; sizv];
    let old = lso.val.elements();
    let n = old.len().min(sizv);
    val[..n].copy_from_slice(&old[..n]);
    let length = io.scanf_string(format, &mut val);
    if length == ERROR as isize {
        lso.val.set_text_strlen(&val[..n]);
        return ERROR;
    }
    let length = length as usize;
    // len = length: the elements as they are, the NUL counted. C's
    // `val[length] = 0` lands past LEN, where VAL here holds nothing.
    lso.val.set_text_strlen(&val[..length]);
    if io.pact() {
        return OK;
    }
    // In @init handler, no processing, enforce monitor updates.
    let reset = rec_gbl_reset_alarms(common);
    io.post_alarm_reset(common, &reset);
    let mut monitor_mask = if reset.alarm_changed || reset.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
    };
    let olen = lso.oval.len();
    if lso.val.len() != olen || lso.oval.elements()[..] != lso.val.elements()[..] {
        monitor_mask |= EventMask::VALUE | EventMask::LOG;
        lso.oval.copy_from(&lso.val);
    }
    if lso.val.len() != olen {
        io.post_events(
            "LEN",
            EpicsValue::ULong(lso.val.len()),
            EventMask::VALUE | EventMask::LOG,
        );
    }
    if lso.mpst == POST_ALWAYS {
        monitor_mask |= EventMask::VALUE;
    }
    if lso.apst == POST_ALWAYS {
        monitor_mask |= EventMask::LOG;
    }
    if !monitor_mask.is_empty() {
        io.post_events(
            "VAL",
            EpicsValue::CharArray(lso.val.elements().clone()),
            monitor_mask,
        );
    }
    OK
}

fn write_data(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let lso = lso(record);

    if format.type_ != Dbf::String {
        return ERROR;
    }
    io.printf_string(format, lso.val.elements())
}

struct Lso;

impl StreamRecordType for Lso {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read_data),
            write_data: Some(write_data),
            map_error: true,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}
