use crate::error::{CaError, CaResult};
use std::time::{Duration, SystemTime};

use super::{DbFieldType, EpicsValue};

// db_access.h constants
const MAX_UNITS_SIZE: usize = 8;
const MAX_ENUM_STATES: usize = 16;
const MAX_ENUM_STRING_SIZE: usize = 26;

const EPICS_UNIX_EPOCH_OFFSET_SECS: u64 = 631_152_000;

pub fn serialize_dbr(
    dbr_type: u16,
    value: &EpicsValue,
    status: u16,
    severity: u16,
    timestamp: SystemTime,
) -> CaResult<Vec<u8>> {
    let native = super::native_type_for_dbr(dbr_type)?;
    let val_bytes = convert_and_serialize(native, value)?;
    match dbr_type {
        0..=6 => Ok(val_bytes),
        7..=13 => serialize_sts(native, &val_bytes, status, severity),
        14..=20 => serialize_time(native, &val_bytes, status, severity, timestamp),
        21..=27 => serialize_gr_ctrl(native, &val_bytes, status, severity, false),
        28..=34 => serialize_gr_ctrl(native, &val_bytes, status, severity, true),
        _ => Err(CaError::UnsupportedType(dbr_type)),
    }
}

fn epics_timestamp_parts(timestamp: SystemTime) -> (u32, u32) {
    let unix = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let sec_past_epoch = unix
        .as_secs()
        .saturating_sub(EPICS_UNIX_EPOCH_OFFSET_SECS)
        .min(u32::MAX as u64) as u32;
    (sec_past_epoch, unix.subsec_nanos())
}

/// Convert value to the target native type and serialize to bytes.
fn convert_and_serialize(native: DbFieldType, value: &EpicsValue) -> CaResult<Vec<u8>> {
    if value.dbr_type() == native {
        return Ok(value.to_bytes());
    }
    Ok(value.convert_to(native).to_bytes())
}

/// RISC alignment padding bytes required between metadata header and value.
fn sts_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // status(2)+severity(2) = 4 → short/enum need 0 pad, char needs 1 pad byte
        DbFieldType::Char => &[0],
        // double needs 2 pad bytes to reach 8-byte alignment: sts(4)+pad(2) = 6? No.
        // C struct: sts_double has RISC_pad (dbr_short_t) between severity and value
        DbFieldType::Double => &[0, 0],
        _ => &[],
    }
}

/// RISC alignment padding bytes for TIME structs (after 12-byte metadata header).
fn time_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // 12 bytes header → short/enum need 2-pad, char needs 2+1=3 pad
        DbFieldType::Short | DbFieldType::Enum => &[0, 0],
        DbFieldType::Char => &[0, 0, 0],
        // double: 12 → pad 4 to reach 16-byte boundary for 8-byte double
        DbFieldType::Double => &[0, 0, 0, 0],
        _ => &[],
    }
}

fn serialize_sts(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
) -> CaResult<Vec<u8>> {
    let pad = sts_pad(native);
    let mut buf = Vec::with_capacity(4 + pad.len() + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());
    buf.extend_from_slice(pad);
    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

fn serialize_time(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
    timestamp: SystemTime,
) -> CaResult<Vec<u8>> {
    let (secs, nanos) = epics_timestamp_parts(timestamp);
    let pad = time_pad(native);
    let mut buf = Vec::with_capacity(12 + pad.len() + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());
    buf.extend_from_slice(&secs.to_be_bytes());
    buf.extend_from_slice(&nanos.to_be_bytes());
    buf.extend_from_slice(pad);
    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Serialize value with GR or CTRL metadata (zeroed) matching the C struct layout in db_access.h.
/// GR types include display/alarm limits; CTRL types add control limits.
fn serialize_gr_ctrl(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
    ctrl: bool,
) -> CaResult<Vec<u8>> {
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            // GR/CTRL String: "not implemented; use struct_dbr_sts_string" per db_access.h
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            // no_str: u16 + strs: char[16][26] — same for GR and CTRL
            buf.extend_from_slice(&0u16.to_be_bytes());
            buf.extend_from_slice(&[0u8; MAX_ENUM_STATES * MAX_ENUM_STRING_SIZE]);
        }
        DbFieldType::Float => {
            // precision(2) + RISC_pad(2) + units[8] + 6 or 8 limits (f32)
            buf.extend_from_slice(&[0u8; 4]); // precision + pad
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 4]);
        }
        DbFieldType::Double => {
            // precision(2) + RISC_pad(2) + units[8] + 6 or 8 limits (f64)
            buf.extend_from_slice(&[0u8; 4]); // precision + pad
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 8]);
        }
        DbFieldType::Short => {
            // units[8] + 6 or 8 limits (i16)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 2]);
        }
        DbFieldType::Long => {
            // units[8] + 6 or 8 limits (i32)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 4]);
        }
        DbFieldType::Char => {
            // units[8] + 6 or 8 limits (u8) + RISC_pad(1)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits]);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Encode a DBR response from a Snapshot. GR/CTRL types include real metadata.
/// Plain/Sts/Time are byte-identical to serialize_dbr output.
pub fn encode_dbr(
    dbr_type: u16,
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let native = super::native_type_for_dbr(dbr_type)?;
    let val_bytes = convert_and_serialize(native, &snapshot.value)?;
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;

    match dbr_type {
        0..=6 => Ok(val_bytes),
        7..=13 => serialize_sts(native, &val_bytes, status, severity),
        14..=20 => serialize_time(native, &val_bytes, status, severity, snapshot.timestamp),
        21..=27 => encode_gr(native, &val_bytes, snapshot),
        28..=34 => encode_ctrl(native, &val_bytes, snapshot),
        _ => Err(CaError::UnsupportedType(dbr_type)),
    }
}

/// Encode GR (graphic/display) metadata + value.
fn encode_gr(
    native: DbFieldType,
    val_bytes: &[u8],
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            encode_enum_metadata(&mut buf, snapshot);
        }
        DbFieldType::Float => {
            encode_prec_units_limits_f32(&mut buf, snapshot, 6);
        }
        DbFieldType::Double => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 6);
        }
        DbFieldType::Short => {
            encode_units_limits_i16(&mut buf, snapshot, 6);
        }
        DbFieldType::Long => {
            encode_units_limits_i32(&mut buf, snapshot, 6);
        }
        DbFieldType::Char => {
            encode_units_limits_u8(&mut buf, snapshot, 6);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Encode CTRL (control) metadata + value. Same as GR but with 8 limits (adds upper/lower ctrl).
fn encode_ctrl(
    native: DbFieldType,
    val_bytes: &[u8],
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;
    let mut buf = Vec::with_capacity(96 + val_bytes.len());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            buf.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            encode_enum_metadata(&mut buf, snapshot);
        }
        DbFieldType::Float => {
            encode_prec_units_limits_f32(&mut buf, snapshot, 8);
        }
        DbFieldType::Double => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 8);
        }
        DbFieldType::Short => {
            encode_units_limits_i16(&mut buf, snapshot, 8);
        }
        DbFieldType::Long => {
            encode_units_limits_i32(&mut buf, snapshot, 8);
        }
        DbFieldType::Char => {
            encode_units_limits_u8(&mut buf, snapshot, 8);
            buf.push(0); // RISC_pad
        }
    }

    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

/// Write units field (8 bytes, null-padded).
fn encode_units(buf: &mut Vec<u8>, snapshot: &crate::server::snapshot::Snapshot) {
    let mut units_buf = [0u8; MAX_UNITS_SIZE];
    if let Some(ref disp) = snapshot.display {
        let bytes = disp.units.as_bytes();
        let len = bytes.len().min(MAX_UNITS_SIZE - 1);
        units_buf[..len].copy_from_slice(&bytes[..len]);
    }
    buf.extend_from_slice(&units_buf);
}

/// Get the 6 display limits + optional 2 control limits from snapshot.
fn get_limits(snapshot: &crate::server::snapshot::Snapshot, n_limits: usize) -> [f64; 8] {
    let mut limits = [0.0f64; 8];
    if let Some(ref disp) = snapshot.display {
        limits[0] = disp.upper_disp_limit;
        limits[1] = disp.lower_disp_limit;
        limits[2] = disp.upper_alarm_limit;
        limits[3] = disp.upper_warning_limit;
        limits[4] = disp.lower_warning_limit;
        limits[5] = disp.lower_alarm_limit;
    }
    if n_limits > 6 {
        if let Some(ref ctrl) = snapshot.control {
            limits[6] = ctrl.upper_ctrl_limit;
            limits[7] = ctrl.lower_ctrl_limit;
        }
    }
    limits
}

/// precision(2) + pad(2) + units(8) + n limits as f64
fn encode_prec_units_limits_f64(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    let prec = snapshot.display.as_ref().map(|d| d.precision).unwrap_or(0);
    buf.extend_from_slice(&prec.to_be_bytes());
    buf.extend_from_slice(&[0, 0]); // RISC_pad
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&l.to_be_bytes());
    }
}

/// precision(2) + pad(2) + units(8) + n limits as f32
fn encode_prec_units_limits_f32(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    let prec = snapshot.display.as_ref().map(|d| d.precision).unwrap_or(0);
    buf.extend_from_slice(&prec.to_be_bytes());
    buf.extend_from_slice(&[0, 0]); // RISC_pad
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as f32).to_be_bytes());
    }
}

/// units(8) + n limits as i16
fn encode_units_limits_i16(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as i16).to_be_bytes());
    }
}

/// units(8) + n limits as i32
fn encode_units_limits_i32(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.extend_from_slice(&(*l as i32).to_be_bytes());
    }
}

/// units(8) + n limits as u8
fn encode_units_limits_u8(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        buf.push(*l as u8);
    }
}

/// no_str(2) + strs(16x26)
fn encode_enum_metadata(buf: &mut Vec<u8>, snapshot: &crate::server::snapshot::Snapshot) {
    if let Some(ref ei) = snapshot.enums {
        let no_str = ei.strings.len().min(MAX_ENUM_STATES) as u16;
        buf.extend_from_slice(&no_str.to_be_bytes());
        for i in 0..MAX_ENUM_STATES {
            let mut slot = [0u8; MAX_ENUM_STRING_SIZE];
            if let Some(s) = ei.strings.get(i) {
                let bytes = s.as_bytes();
                let len = bytes.len().min(MAX_ENUM_STRING_SIZE - 1);
                slot[..len].copy_from_slice(&bytes[..len]);
            }
            buf.extend_from_slice(&slot);
        }
    } else {
        // No enum info — zero everything (backward compatible)
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&[0u8; MAX_ENUM_STATES * MAX_ENUM_STRING_SIZE]);
    }
}

// ---------------------------------------------------------------------------
// Decode (deserialize) DBR wire bytes → Snapshot
// ---------------------------------------------------------------------------

use crate::server::snapshot::*;

fn read_u16(data: &[u8], off: usize) -> CaResult<u16> {
    if off + 2 > data.len() {
        return Err(CaError::Protocol("buffer too short for u16".into()));
    }
    Ok(u16::from_be_bytes([data[off], data[off + 1]]))
}

fn read_i16(data: &[u8], off: usize) -> CaResult<i16> {
    if off + 2 > data.len() {
        return Err(CaError::Protocol("buffer too short for i16".into()));
    }
    Ok(i16::from_be_bytes([data[off], data[off + 1]]))
}

fn read_u32(data: &[u8], off: usize) -> CaResult<u32> {
    if off + 4 > data.len() {
        return Err(CaError::Protocol("buffer too short for u32".into()));
    }
    Ok(u32::from_be_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn read_i32(data: &[u8], off: usize) -> CaResult<i32> {
    if off + 4 > data.len() {
        return Err(CaError::Protocol("buffer too short for i32".into()));
    }
    Ok(i32::from_be_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn read_f32(data: &[u8], off: usize) -> CaResult<f32> {
    if off + 4 > data.len() {
        return Err(CaError::Protocol("buffer too short for f32".into()));
    }
    Ok(f32::from_be_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn read_f64(data: &[u8], off: usize) -> CaResult<f64> {
    if off + 8 > data.len() {
        return Err(CaError::Protocol("buffer too short for f64".into()));
    }
    Ok(f64::from_be_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
        data[off + 4],
        data[off + 5],
        data[off + 6],
        data[off + 7],
    ]))
}

fn read_string(data: &[u8], off: usize, max_len: usize) -> String {
    let end = data.len().min(off + max_len);
    if off >= end {
        return String::new();
    }
    let slice = &data[off..end];
    let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..nul]).into_owned()
}

fn epics_secs_to_system_time(secs: u32, nanos: u32) -> SystemTime {
    let unix_secs = secs as u64 + EPICS_UNIX_EPOCH_OFFSET_SECS;
    SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs) + Duration::from_nanos(nanos as u64)
}

/// Decode a DBR wire response into a Snapshot.
///
/// This is the inverse of `encode_dbr()`. It parses status/severity, timestamp,
/// display/control metadata, and the value payload from the raw bytes.
pub fn decode_dbr(dbr_type: u16, data: &[u8], count: usize) -> CaResult<Snapshot> {
    let native = super::native_type_for_dbr(dbr_type)?;
    match dbr_type {
        0..=6 => {
            let value = EpicsValue::from_bytes_array(native, data, count)?;
            Ok(Snapshot::new(value, 0, 0, SystemTime::UNIX_EPOCH))
        }
        7..=13 => decode_sts(native, data, count),
        14..=20 => decode_time(native, data, count),
        21..=27 => decode_gr_ctrl(native, data, count, false),
        28..=34 => decode_gr_ctrl(native, data, count, true),
        _ => Err(CaError::UnsupportedType(dbr_type)),
    }
}

fn decode_sts(native: DbFieldType, data: &[u8], count: usize) -> CaResult<Snapshot> {
    let status = read_u16(data, 0)?;
    let severity = read_u16(data, 2)?;
    let pad_len = sts_pad(native).len();
    let val_off = 4 + pad_len;
    let value = EpicsValue::from_bytes_array(native, &data[val_off..], count)?;
    Ok(Snapshot::new(
        value,
        status,
        severity,
        SystemTime::UNIX_EPOCH,
    ))
}

fn decode_time(native: DbFieldType, data: &[u8], count: usize) -> CaResult<Snapshot> {
    let status = read_u16(data, 0)?;
    let severity = read_u16(data, 2)?;
    let secs = read_u32(data, 4)?;
    let nanos = read_u32(data, 8)?;
    let timestamp = epics_secs_to_system_time(secs, nanos);
    let pad_len = time_pad(native).len();
    let val_off = 12 + pad_len;
    let value = EpicsValue::from_bytes_array(native, &data[val_off..], count)?;
    Ok(Snapshot::new(value, status, severity, timestamp))
}

fn decode_gr_ctrl(
    native: DbFieldType,
    data: &[u8],
    count: usize,
    ctrl: bool,
) -> CaResult<Snapshot> {
    let status = read_u16(data, 0)?;
    let severity = read_u16(data, 2)?;
    let mut off = 4;

    let mut display = None;
    let mut control = None;
    let mut enums = None;

    match native {
        DbFieldType::String => {
            off += sts_pad(native).len();
        }
        DbFieldType::Enum => {
            let (ei, new_off) = decode_enum_metadata(data, off)?;
            enums = Some(ei);
            off = new_off;
        }
        DbFieldType::Float => {
            let precision = read_i16(data, off)?;
            off += 4; // precision(2) + pad(2)
            let units = read_string(data, off, MAX_UNITS_SIZE);
            off += MAX_UNITS_SIZE;
            let n_limits = if ctrl { 8 } else { 6 };
            let mut limits = [0.0f64; 8];
            for i in 0..n_limits {
                limits[i] = read_f32(data, off)? as f64;
                off += 4;
            }
            display = Some(DisplayInfo {
                units,
                precision,
                upper_disp_limit: limits[0],
                lower_disp_limit: limits[1],
                upper_alarm_limit: limits[2],
                upper_warning_limit: limits[3],
                lower_warning_limit: limits[4],
                lower_alarm_limit: limits[5],
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
        DbFieldType::Double => {
            let precision = read_i16(data, off)?;
            off += 4; // precision(2) + pad(2)
            let units = read_string(data, off, MAX_UNITS_SIZE);
            off += MAX_UNITS_SIZE;
            let n_limits = if ctrl { 8 } else { 6 };
            let mut limits = [0.0f64; 8];
            for i in 0..n_limits {
                limits[i] = read_f64(data, off)?;
                off += 8;
            }
            display = Some(DisplayInfo {
                units,
                precision,
                upper_disp_limit: limits[0],
                lower_disp_limit: limits[1],
                upper_alarm_limit: limits[2],
                upper_warning_limit: limits[3],
                lower_warning_limit: limits[4],
                lower_alarm_limit: limits[5],
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
        DbFieldType::Short => {
            let units = read_string(data, off, MAX_UNITS_SIZE);
            off += MAX_UNITS_SIZE;
            let n_limits = if ctrl { 8 } else { 6 };
            let mut limits = [0.0f64; 8];
            for i in 0..n_limits {
                limits[i] = read_i16(data, off)? as f64;
                off += 2;
            }
            display = Some(DisplayInfo {
                units,
                precision: 0,
                upper_disp_limit: limits[0],
                lower_disp_limit: limits[1],
                upper_alarm_limit: limits[2],
                upper_warning_limit: limits[3],
                lower_warning_limit: limits[4],
                lower_alarm_limit: limits[5],
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
        DbFieldType::Long => {
            let units = read_string(data, off, MAX_UNITS_SIZE);
            off += MAX_UNITS_SIZE;
            let n_limits = if ctrl { 8 } else { 6 };
            let mut limits = [0.0f64; 8];
            for i in 0..n_limits {
                limits[i] = read_i32(data, off)? as f64;
                off += 4;
            }
            display = Some(DisplayInfo {
                units,
                precision: 0,
                upper_disp_limit: limits[0],
                lower_disp_limit: limits[1],
                upper_alarm_limit: limits[2],
                upper_warning_limit: limits[3],
                lower_warning_limit: limits[4],
                lower_alarm_limit: limits[5],
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
        DbFieldType::Char => {
            let units = read_string(data, off, MAX_UNITS_SIZE);
            off += MAX_UNITS_SIZE;
            let n_limits = if ctrl { 8 } else { 6 };
            let mut limits = [0.0f64; 8];
            for i in 0..n_limits {
                if off < data.len() {
                    limits[i] = data[off] as f64;
                }
                off += 1;
            }
            off += 1; // RISC_pad
            display = Some(DisplayInfo {
                units,
                precision: 0,
                upper_disp_limit: limits[0],
                lower_disp_limit: limits[1],
                upper_alarm_limit: limits[2],
                upper_warning_limit: limits[3],
                lower_warning_limit: limits[4],
                lower_alarm_limit: limits[5],
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
    }

    let value = EpicsValue::from_bytes_array(native, &data[off..], count)?;
    let mut snap = Snapshot::new(value, status, severity, SystemTime::UNIX_EPOCH);
    snap.display = display;
    snap.control = control;
    snap.enums = enums;
    Ok(snap)
}

fn decode_enum_metadata(data: &[u8], off: usize) -> CaResult<(EnumInfo, usize)> {
    let no_str = read_u16(data, off)? as usize;
    let mut pos = off + 2;
    let mut strings = Vec::with_capacity(no_str.min(MAX_ENUM_STATES));
    for i in 0..MAX_ENUM_STATES {
        let s = read_string(data, pos, MAX_ENUM_STRING_SIZE);
        if i < no_str {
            strings.push(s);
        }
        pos += MAX_ENUM_STRING_SIZE;
    }
    Ok((EnumInfo { strings }, pos))
}
