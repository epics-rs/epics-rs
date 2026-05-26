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
    // DBR_CLASS_NAME (38) carries a single 40-byte string with the
    // record's recordType. The metadata-free encoding mirrors the C
    // `dbChannel_classify` reply path. `value` here is expected to
    // already be a String. Used only by `serialize_dbr` callers; the
    // server-driven path (`encode_dbr`) reads `Snapshot::class_name`.
    if dbr_type == super::DBR_CLASS_NAME {
        let mut buf = [0u8; 40];
        if let EpicsValue::String(s) = value {
            let bytes = s.as_bytes();
            let len = bytes.len().min(39);
            buf[..len].copy_from_slice(&bytes[..len]);
        }
        return Ok(buf.to_vec());
    }
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
pub(crate) fn sts_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // status(2)+severity(2) = 4 → short/enum need 0 pad.
        // `dbr_sts_char` (db_access.h:218-223): `dbr_char_t RISC_pad`
        // (epicsInt8, 1 byte) between severity and value.
        DbFieldType::Char => &[0],
        // `dbr_sts_double` (db_access.h:233-238): `dbr_long_t RISC_pad`
        // (epicsInt32, **4 bytes** — `db_access.h:45 typedef epicsInt32
        // dbr_long_t`) between severity and value. Layout is
        // status(2)+severity(2)+RISC_pad(4)+value(8) → value at offset 8.
        // Int64/UInt64 are served over CA as DBR_DOUBLE so they share the pad.
        DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 => &[0, 0, 0, 0],
        _ => &[],
    }
}

/// RISC alignment padding bytes for TIME structs (after 12-byte metadata header).
pub(crate) fn time_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // 12 bytes header → short/enum need 2-pad, char needs 2+1=3 pad
        DbFieldType::Short | DbFieldType::Enum => &[0, 0],
        DbFieldType::Char => &[0, 0, 0],
        // double: 12 → pad 4 to reach 16-byte boundary for 8-byte double
        DbFieldType::Double => &[0, 0, 0, 0],
        _ => &[],
    }
}

/// Metadata byte count that precedes `value[0]` for a GR (`ctrl=false`)
/// or CTRL (`ctrl=true`) DBR struct. CA-FR-7: this is the single source
/// of truth shared with [`serialize_gr_ctrl`] / [`encode_gr`] /
/// [`encode_ctrl`] — every component below is the exact byte sequence
/// those writers emit before the value, so the sizer cannot drift from
/// the encoder. `n_limits` is 6 for GR (display/alarm limits) and 8 for
/// CTRL (plus upper/lower control limits).
fn gr_ctrl_meta_size(native: DbFieldType, ctrl: bool) -> usize {
    let n_limits = if ctrl { 8 } else { 6 };
    // status(2) + severity(2) is common to every GR/CTRL struct.
    let head = 4;
    match native {
        // "not implemented; use dbr_sts_string" — only the common head.
        DbFieldType::String => head + sts_pad(native).len(),
        // no_str(u16) + char[MAX_ENUM_STATES][MAX_ENUM_STRING_SIZE].
        DbFieldType::Enum => head + 2 + MAX_ENUM_STATES * MAX_ENUM_STRING_SIZE,
        // precision(2) + RISC_pad(2) + units[8] + n limits (f32).
        DbFieldType::Float => head + 4 + MAX_UNITS_SIZE + n_limits * 4,
        // precision(2) + RISC_pad(2) + units[8] + n limits (f64). Int64/
        // UInt64 have no CA GR/CTRL type and reuse the Double layout.
        DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 => {
            head + 4 + MAX_UNITS_SIZE + n_limits * 8
        }
        // units[8] + n limits (i16).
        DbFieldType::Short => head + MAX_UNITS_SIZE + n_limits * 2,
        // units[8] + n limits (i32).
        DbFieldType::Long => head + MAX_UNITS_SIZE + n_limits * 4,
        // units[8] + n limits (u8) + RISC_pad(1).
        DbFieldType::Char => head + MAX_UNITS_SIZE + n_limits + 1,
    }
}

/// Number of metadata bytes that precede `value[0]` for a DBR type —
/// the single size owner mirrored from the serializers in this module.
/// CA-FR-7: `dbr_buffer_size` derives payload sizing from this so the
/// explicit-count pad/truncate and no-read-access frame paths match the
/// bytes the encoder actually writes (C `dbr_size_n` parity), instead of
/// a parallel table that drifted on TIME/GR/CTRL layouts.
///
/// `DBR_CLASS_NAME` (38) is intentionally not handled here — it carries
/// no `value[]` array and is sized as a flat 40-byte string by the
/// caller. Returns the value-preceding metadata length for everything
/// in the plain/STS/TIME/GR/CTRL ranges plus the alarm-ack variants.
pub(crate) fn dbr_meta_size(dbr_type: u16, native: DbFieldType) -> usize {
    match dbr_type {
        // Plain value, no metadata.
        0..=6 => 0,
        // STS: status(2) + severity(2) + per-type RISC pad.
        7..=13 => 4 + sts_pad(native).len(),
        // TIME: status(2) + severity(2) + stamp(8) + per-type RISC pad.
        14..=20 => 12 + time_pad(native).len(),
        // GR: display/alarm limits.
        21..=27 => gr_ctrl_meta_size(native, false),
        // CTRL: GR plus control limits.
        28..=34 => gr_ctrl_meta_size(native, true),
        // PUT_ACKT / PUT_ACKS carry only a bare u16 value (no metadata);
        // the server emits them with `Ok(val_bytes)`.
        super::DBR_PUT_ACKT | super::DBR_PUT_ACKS => 0,
        // STSACK_STRING: status(2) + severity(2) + ackt(2) + acks(2)
        // before the 40-byte value string.
        super::DBR_STSACK_STRING => 8,
        // CLASS_NAME and anything else: no value-preceding metadata model.
        _ => 0,
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
        // Int64/UInt64 have no CA GR/CTRL type — this arm is unreachable in
        // normal CA paths. Use same layout as Double
        // (precision(2)+pad(2)+units(8)+n*f64 limits).
        DbFieldType::Int64 | DbFieldType::UInt64 => {
            buf.extend_from_slice(&[0u8; 4]); // precision + pad
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 8]);
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
    // CLASS_NAME (38) emits a fixed 40-byte string from
    // snapshot.class_name and ignores snapshot.value entirely.
    // Early-return BEFORE convert_and_serialize so a waveform PV's
    // value (potentially N elements wide) is not Display'd into a
    // throwaway joined string. Mirrors the same shortcut in
    // serialize_dbr (line ~25).
    if dbr_type == super::DBR_CLASS_NAME {
        let mut buf = [0u8; 40];
        if let Some(ref name) = snapshot.class_name {
            let bytes = name.as_bytes();
            let len = bytes.len().min(39);
            buf[..len].copy_from_slice(&bytes[..len]);
        }
        return Ok(buf.to_vec());
    }
    let native = super::native_type_for_dbr(dbr_type)?;
    // R57: a `*_STRING` request of a Double/Float field must honor the
    // record's precision (C `getDoubleString` → `cvtDoubleToString`).
    // `EpicsValue::convert_to(String)` (value.rs) has no record context, so
    // route string requests through the precision-aware converter here.
    let val_bytes = convert_and_serialize(native, &snapshot.value)?;
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;

    match dbr_type {
        0..=6 => Ok(val_bytes),
        7..=13 => serialize_sts(native, &val_bytes, status, severity),
        14..=20 => serialize_time(native, &val_bytes, status, severity, snapshot.timestamp),
        21..=27 => encode_gr(native, &val_bytes, snapshot),
        28..=34 => encode_ctrl(native, &val_bytes, snapshot),
        // PUT_ACKT (35) and PUT_ACKS (36) are write-only on the wire:
        // the server should never produce them in a read response. We
        // expose a no-op encoding so callers that round-trip through
        // encode_dbr (e.g. forwarders) don't fail loudly.
        super::DBR_PUT_ACKT | super::DBR_PUT_ACKS => Ok(val_bytes),
        // STSACK_STRING — alarm acknowledge string response. Layout:
        //   status(2) severity(2) ackt(2) acks(2) value(40) = 48 bytes.
        // ackt/acks are taken from snapshot.alarm if available; the
        // value itself comes from the encoded native String.
        super::DBR_STSACK_STRING => {
            let ackt = snapshot.alarm.ackt.unwrap_or(0);
            let acks = snapshot.alarm.acks.unwrap_or(0);
            let mut buf = Vec::with_capacity(8 + val_bytes.len());
            buf.extend_from_slice(&status.to_be_bytes());
            buf.extend_from_slice(&severity.to_be_bytes());
            buf.extend_from_slice(&ackt.to_be_bytes());
            buf.extend_from_slice(&acks.to_be_bytes());
            buf.extend_from_slice(&val_bytes);
            Ok(buf)
        }
        // CLASS_NAME (38) handled by the early-return above.
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
        // Int64/UInt64 have no CA GR type; use Double layout.
        DbFieldType::Int64 | DbFieldType::UInt64 => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 6);
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
        // Int64/UInt64 have no CA CTRL type; use Double layout.
        DbFieldType::Int64 | DbFieldType::UInt64 => {
            encode_prec_units_limits_f64(&mut buf, snapshot, 8);
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

/// units(8) + n limits as u8 (DBF_CHAR limits, transmitted as
/// SIGNED `epicsInt8` per the libca spec — `7cb80d5a1`). Encoding
/// path: `f64 limit → i8 (saturating) → u8 bit-pattern → wire byte`.
/// Without the i8 intermediate `(-10.0_f64) as u8` saturates to 0
/// in Rust, silently destroying negative DRVL/LOPR/HOPR/HIHI on
/// DBF_CHAR records. P-8 finding (BUG_ARCHAEOLOGY).
fn encode_units_limits_u8(
    buf: &mut Vec<u8>,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    encode_units(buf, snapshot);
    let limits = get_limits(snapshot, n_limits);
    for l in &limits[..n_limits] {
        // Saturating cast f64 → i8 → wire byte. `as i8` on f64 in
        // Rust ≥1.45 saturates to [i8::MIN, i8::MAX] (no UB).
        buf.push((*l as i8) as u8);
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
    // DBR_CLASS_NAME (38): a fixed-40-byte string with the record's
    // recordType. Decoded into Snapshot.class_name so callers can read
    // it without touching `value` (which stays empty/Default).
    // Strict length check — a short payload is a malformed CA frame,
    // not a partial result we should silently truncate to garbage.
    if dbr_type == super::DBR_CLASS_NAME {
        if data.len() < 40 {
            return Err(CaError::Protocol(format!(
                "DBR_CLASS_NAME requires 40-byte payload, got {}",
                data.len()
            )));
        }
        let name = read_string(data, 0, 40);
        let mut snap = Snapshot::new(
            EpicsValue::String(name.clone()),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        );
        snap.class_name = Some(name);
        return Ok(snap);
    }
    // DBR_STSACK_STRING (37): alarm-acknowledge string. Layout per
    // `dbr_stsack_string` (db_access.h:184-190):
    //   status(2) severity(2) ackt(2) acks(2) value(40) = 48 bytes.
    // The inverse of the `encode_dbr` STSACK_STRING arm — without
    // this the encode/decode pair is asymmetric (M-6).
    if dbr_type == super::DBR_STSACK_STRING {
        if data.len() < 48 {
            return Err(CaError::Protocol(format!(
                "DBR_STSACK_STRING requires 48-byte payload, got {}",
                data.len()
            )));
        }
        let status = read_u16(data, 0)?;
        let severity = read_u16(data, 2)?;
        let ackt = read_u16(data, 4)?;
        let acks = read_u16(data, 6)?;
        let value = read_string(data, 8, 40);
        let mut snap = Snapshot::new(
            EpicsValue::String(value),
            status,
            severity,
            SystemTime::UNIX_EPOCH,
        );
        snap.alarm.ackt = Some(ackt);
        snap.alarm.acks = Some(acks);
        return Ok(snap);
    }
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                    // Decode as SIGNED i8 → f64 (DBF_CHAR is
                    // epicsInt8 per libca 7cb80d5a1). The previous
                    // `data[off] as f64` route read 0xF6 as 246.0
                    // instead of -10.0, silently flipping signs on
                    // CTRL_CHAR / GR_CHAR negative DRVL/LOPR limits.
                    limits[i] = (data[off] as i8) as f64;
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
                ..Default::default()
            });
            if ctrl {
                control = Some(ControlInfo {
                    upper_ctrl_limit: limits[6],
                    lower_ctrl_limit: limits[7],
                });
            }
        }
        // Int64/UInt64 have no CA GR/CTRL type; decode as Double layout.
        DbFieldType::Int64 | DbFieldType::UInt64 => {
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
                ..Default::default()
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

#[cfg(test)]
mod wire_format_tests {
    use super::*;
    use crate::types::dbr::{
        DBR_CTRL_DOUBLE, DBR_DOUBLE, DBR_GR_DOUBLE, DBR_STS_DOUBLE, DBR_TIME_DOUBLE,
        dbr_buffer_size,
    };

    /// CA-FR-7 structural invariant: the encoded DBR length must equal
    /// `dbr_buffer_size` for every (dbr_type, native, count). This pins
    /// the sizer ([`dbr_meta_size`]) to the bytes the serializer
    /// actually writes, so the two can never drift again. Covers plain /
    /// STS / TIME / GR / CTRL layers for all seven CA native types, at
    /// scalar and multi-element counts.
    #[test]
    fn metadata_matches_encoded_length() {
        // (native, scalar value, 3-element array value)
        let cases: &[(DbFieldType, EpicsValue, EpicsValue)] = &[
            (
                DbFieldType::String,
                EpicsValue::String("x".into()),
                EpicsValue::StringArray(vec!["x".into(), "y".into(), "z".into()]),
            ),
            (
                DbFieldType::Short,
                EpicsValue::Short(7),
                EpicsValue::ShortArray(vec![1, 2, 3]),
            ),
            (
                DbFieldType::Float,
                EpicsValue::Float(1.5),
                EpicsValue::FloatArray(vec![1.0, 2.0, 3.0]),
            ),
            (
                DbFieldType::Enum,
                EpicsValue::Enum(2),
                EpicsValue::EnumArray(vec![0, 1, 2]),
            ),
            (
                DbFieldType::Char,
                EpicsValue::Char(9),
                EpicsValue::CharArray(vec![1, 2, 3]),
            ),
            (
                DbFieldType::Long,
                EpicsValue::Long(11),
                EpicsValue::LongArray(vec![1, 2, 3]),
            ),
            (
                DbFieldType::Double,
                EpicsValue::Double(2.5),
                EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
            ),
        ];
        let now = SystemTime::now();
        for (native, scalar, array) in cases {
            let base = *native as u16;
            // layer 0=plain, 1=STS, 2=TIME, 3=GR, 4=CTRL
            for layer in 0u16..=4 {
                let dbr_type = base + 7 * layer;
                for (value, count) in [(scalar, 1usize), (array, 3usize)] {
                    let encoded = serialize_dbr(dbr_type, value, 0, 0, now)
                        .expect("serialize_dbr")
                        .len();
                    let sized = dbr_buffer_size(dbr_type, *native, count);
                    assert_eq!(
                        encoded, sized,
                        "len mismatch dbr_type={dbr_type} native={native:?} count={count}"
                    );
                }
            }
        }
    }

    /// The server-driven `encode_dbr` path (real metadata) must size the
    /// same as `dbr_buffer_size` — proving both encoders share the one
    /// `dbr_meta_size` owner, not just the zeroed `serialize_dbr` path.
    #[test]
    fn encode_dbr_length_matches_sizer() {
        use crate::server::snapshot::Snapshot;
        let layers = [
            DBR_DOUBLE,
            DBR_STS_DOUBLE,
            DBR_TIME_DOUBLE,
            DBR_GR_DOUBLE,
            DBR_CTRL_DOUBLE,
        ];
        let snap = Snapshot::new(EpicsValue::Double(3.25), 0, 0, SystemTime::now());
        for dbr_type in layers {
            let encoded = encode_dbr(dbr_type, &snap).expect("encode_dbr").len();
            let sized = dbr_buffer_size(dbr_type, DbFieldType::Double, 1);
            assert_eq!(
                encoded, sized,
                "encode_dbr len mismatch dbr_type={dbr_type}"
            );
        }
    }

    /// C-1: `DBR_STS_DOUBLE` (type 13) wire layout is
    /// `status(2) + severity(2) + RISC_pad(4) + value(8)` — the
    /// `RISC_pad` is `dbr_long_t` (epicsInt32, 4 bytes) per
    /// `db_access.h:233-238`. Total 16 bytes, value at offset 8.
    #[test]
    fn sts_double_value_at_offset_8() {
        let v = EpicsValue::Double(3.5);
        let buf = serialize_dbr(
            super::super::DBR_STS_DOUBLE,
            &v,
            1,
            2,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            buf.len(),
            16,
            "STS_DOUBLE must be 16 bytes (4 meta + 4 pad + 8 value)"
        );
        // status(2) + severity(2)
        assert_eq!(&buf[0..2], &1u16.to_be_bytes());
        assert_eq!(&buf[2..4], &2u16.to_be_bytes());
        // RISC_pad(4) — all zero
        assert_eq!(&buf[4..8], &[0, 0, 0, 0]);
        // value(8) at offset 8
        assert_eq!(&buf[8..16], &3.5f64.to_be_bytes());
    }

    /// C-1: STS_DOUBLE encode→decode round-trips with the 4-byte pad.
    #[test]
    fn sts_double_round_trip() {
        let v = EpicsValue::Double(-12.75);
        let buf = serialize_dbr(
            super::super::DBR_STS_DOUBLE,
            &v,
            7,
            3,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let snap = decode_dbr(super::super::DBR_STS_DOUBLE, &buf, 1).unwrap();
        assert_eq!(snap.value, EpicsValue::Double(-12.75));
        assert_eq!(snap.alarm.status, 7);
        assert_eq!(snap.alarm.severity, 3);
    }

    /// C-1 cross-check: STS_CHAR keeps its 1-byte `RISC_pad`
    /// (`dbr_sts_char`, db_access.h:218-223) — value at offset 5.
    #[test]
    fn sts_char_value_at_offset_5() {
        let v = EpicsValue::Char(0x41);
        let buf =
            serialize_dbr(super::super::DBR_STS_CHAR, &v, 0, 0, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            buf.len(),
            6,
            "STS_CHAR must be 6 bytes (4 meta + 1 pad + 1 value)"
        );
        assert_eq!(buf[4], 0, "RISC_pad");
        assert_eq!(buf[5], 0x41, "value at offset 5");
    }

    /// STS_SHORT has no RISC pad — value immediately after the
    /// 4-byte status/severity header (`dbr_sts_short`).
    #[test]
    fn sts_short_no_pad() {
        let v = EpicsValue::Short(0x1234);
        let buf = serialize_dbr(
            super::super::DBR_STS_SHORT,
            &v,
            0,
            0,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(buf.len(), 6, "STS_SHORT is 4 meta + 2 value, no pad");
        assert_eq!(&buf[4..6], &0x1234i16.to_be_bytes());
    }

    /// M-6: `DBR_STSACK_STRING` (37) must decode, not just encode.
    /// Layout: status(2) severity(2) ackt(2) acks(2) value(40).
    #[test]
    fn stsack_string_decodes() {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&5u16.to_be_bytes()); // status
        buf.extend_from_slice(&2u16.to_be_bytes()); // severity
        buf.extend_from_slice(&1u16.to_be_bytes()); // ackt
        buf.extend_from_slice(&3u16.to_be_bytes()); // acks
        let mut value = [0u8; 40];
        value[..5].copy_from_slice(b"HIHI\0");
        buf.extend_from_slice(&value);
        assert_eq!(buf.len(), 48);

        let snap = decode_dbr(super::super::DBR_STSACK_STRING, &buf, 1).unwrap();
        assert_eq!(snap.value, EpicsValue::String("HIHI".into()));
        assert_eq!(snap.alarm.status, 5);
        assert_eq!(snap.alarm.severity, 2);
        assert_eq!(snap.alarm.ackt, Some(1));
        assert_eq!(snap.alarm.acks, Some(3));
    }

    /// A short STSACK_STRING payload is a malformed frame, rejected.
    #[test]
    fn stsack_string_short_payload_errors() {
        let buf = [0u8; 16];
        assert!(decode_dbr(super::super::DBR_STSACK_STRING, &buf, 1).is_err());
    }
}
