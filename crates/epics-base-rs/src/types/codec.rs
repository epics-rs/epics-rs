use crate::error::{CaError, CaResult};
use std::time::SystemTime;

use super::{DbFieldType, EpicsValue, PvString, WallTime};

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
    timestamp: impl Into<WallTime>,
) -> CaResult<Vec<u8>> {
    let timestamp: WallTime = timestamp.into();
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

fn epics_timestamp_parts(timestamp: WallTime) -> (u32, u32) {
    let unix = timestamp.since_unix_epoch();
    let sec_past_epoch = unix
        .as_secs()
        .saturating_sub(EPICS_UNIX_EPOCH_OFFSET_SECS)
        .min(u32::MAX as u64) as u32;
    (sec_past_epoch, unix.subsec_nanos())
}

/// Convert value to the target native type and serialize to bytes.
fn convert_and_serialize(native: DbFieldType, value: &EpicsValue) -> CaResult<Vec<u8>> {
    let mut buf = Vec::new();
    convert_and_serialize_into(native, value, &mut buf);
    Ok(buf)
}

/// [`convert_and_serialize`] appending into a caller-owned buffer. When the
/// value already has the requested native type — every monitor on an
/// unconverted field — the bytes go straight onto `dst` with no intermediate.
fn convert_and_serialize_into(native: DbFieldType, value: &EpicsValue, dst: &mut Vec<u8>) {
    if value.dbr_type() == native {
        value.write_into(dst);
    } else {
        value.convert_to(native).write_into(dst);
    }
}

// precision-aware `*_STRING` rendering of a numeric field.
//
// A `*_STRING` DBR request of a Double/Float field is converted by the C
// IOC with `cvtDoubleToString` / `cvtFloatToString` (libCom cvtFast.c)
// using the record's `get_precision` (PREC). `EpicsValue::convert_to` has
// no record context, so the precision-aware conversion lives here where
// `encode_dbr` holds the snapshot.

/// libCom cvtFast.c `frac_multiplier` — fractional scale by precision 0..=8.
const FRAC_MULTIPLIER: [i32; 9] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
];

/// Render a numeric value as its DBR_STRING form, applying `precision` to
/// Float/Double exactly as C `getDoubleString`/`getFloatString` do. Integer
/// and string values keep the plain `convert_to(String)` path (their C
/// conversions carry no precision, so the default `Display` already matches).
fn convert_value_to_dbr_string(
    value: &EpicsValue,
    snapshot: &crate::server::snapshot::Snapshot,
) -> EpicsValue {
    // C precision = record `get_precision` (PREC field), default 6 when the
    // record exposes no `get_precision` RSET (here: no display metadata).
    //
    // A NEGATIVE PREC is not clamped: C hands the `long precision` straight to
    // `cvtDoubleToString(double, char*, epicsUInt16 precision)`
    // (`dbConvert.c:786`), and the implicit conversion REINTERPRETS it — PREC=-1
    // arrives as 65535, which takes the `precision > 8` branch, caps at 17 and
    // renders `%*.*e` (compiled: `caget -S` of a PREC=-1 ai gives
    // ` 3.70000000000000018e+00`). Clamping to 0 printed `4` instead.
    let prec = snapshot
        .display
        .as_ref()
        .map(|d| d.precision as u16)
        .unwrap_or(6);
    match value {
        EpicsValue::Double(v) => EpicsValue::String(cvt_double_to_string(*v, prec).into()),
        EpicsValue::Float(v) => EpicsValue::String(cvt_float_to_string(*v, prec).into()),
        EpicsValue::DoubleArray(a) => EpicsValue::StringArray(
            a.iter()
                .map(|v| cvt_double_to_string(*v, prec).into())
                .collect(),
        ),
        EpicsValue::FloatArray(a) => EpicsValue::StringArray(
            a.iter()
                .map(|v| cvt_float_to_string(*v, prec).into())
                .collect(),
        ),
        // C `getEnumString` (dbConvert.c, `[DBF_ENUM][DBR_STRING]`) hands the
        // value to the field's own string source — the record's `get_enum_str`
        // rset, a menu's choice list, a device's choice list — and renders what
        // comes back. See `enum_label`.
        EpicsValue::Enum(v) => EpicsValue::String(enum_label(snapshot, *v)),
        EpicsValue::EnumArray(a) => {
            EpicsValue::StringArray(a.iter().map(|v| enum_label(snapshot, *v)).collect())
        }
        other => other.convert_to(DbFieldType::String),
    }
}

/// Render an enum index as C's `get_enum_str` does, through the channel's
/// [`crate::server::snapshot::EnumStringForm`] — the one owner
/// of "what string this enum value is".
///
/// There is no decimal fallback, because C has no path that produces one: a
/// value with no matching slot renders as the record's out-of-range sentinel
/// (empty for a menu), and a channel with no enum source at all is `S_db_noRSET`
/// in C — an error, never the number. The port answers empty rather than invent
/// a digit string that no C IOC can emit.
fn enum_label(snapshot: &crate::server::snapshot::Snapshot, idx: u16) -> PvString {
    snapshot
        .enums
        .as_ref()
        .map(|e| e.string_form.render(idx))
        .unwrap_or_default()
}

/// Port of libCom `cvtDoubleToString` (cvtFast.c:111-190).
///
/// `pub(crate)` so records that mirror C's `cvtDoubleToString(value, str,
/// prec)` (e.g. `sseq` refreshing `STRn` from a numeric `DOLn`,
/// sseqRecord.c:676) format through the exact same converter the DBR-string
/// wire encoder uses, instead of a divergent `format!` that rounds
/// differently.
pub(crate) fn cvt_double_to_string(val: f64, precision: u16) -> String {
    if val.is_nan() || precision > 8 || val > 1e7 || val < -1e7 {
        if precision > 8 || val > 1e16 || val < -1e16 {
            let p = precision.min(17) as usize;
            return fmt_exp_c(val, p, p + 7);
        }
        return fmt_fixed_c(val, precision.min(3) as usize);
    }
    let mut val = val;
    let mut out = String::new();
    if val < 0.0 {
        out.push('-');
        val = -val;
    }
    let mut whole = val as i32;
    let ftemp = val - whole as f64;
    let fplace_full = FRAC_MULTIPLIER[precision as usize];
    let mut fraction = (ftemp * fplace_full as f64 * 10.0) as i32;
    fraction = (fraction + 5) / 10; // round half up
    if fraction / fplace_full >= 1 {
        whole += 1;
        fraction -= fplace_full;
    }
    push_fixed_digits(&mut out, whole, fraction, fplace_full, precision);
    out
}

/// Port of libCom `cvtFloatToString` (cvtFast.c:32-109). The fast path runs
/// in f32 to match the C float arithmetic; the overflow branches cast to
/// double exactly as the C `sprintf((double) flt_value)` does.
fn cvt_float_to_string(val: f32, precision: u16) -> String {
    if val.is_nan() || precision > 8 || val > 1e7 || val < -1e7 {
        if precision > 8 || val >= 1e8 || val <= -1e8 {
            let p = precision.min(12) as usize;
            return fmt_exp_c(val as f64, p, p + 6);
        }
        return fmt_fixed_c(val as f64, precision.min(3) as usize);
    }
    let mut val = val;
    let mut out = String::new();
    if val < 0.0 {
        out.push('-');
        val = -val;
    }
    let mut whole = val as i32;
    let ftemp = val - whole as f32;
    let fplace_full = FRAC_MULTIPLIER[precision as usize];
    let mut fraction = (ftemp * fplace_full as f32 * 10.0) as i32;
    fraction = (fraction + 5) / 10; // round half up
    if fraction / fplace_full >= 1 {
        whole += 1;
        fraction -= fplace_full;
    }
    push_fixed_digits(&mut out, whole, fraction, fplace_full, precision);
    out
}

/// Emit the whole-number then fractional digits, shared by the float/double
/// fast paths (cvtFast.c:75-105 / :156-186).
fn push_fixed_digits(
    out: &mut String,
    mut whole: i32,
    mut fraction: i32,
    fplace_full: i32,
    precision: u16,
) {
    let mut got_one = false;
    let mut iplace = 10_000_000i32;
    while iplace >= 1 {
        if whole >= iplace {
            got_one = true;
            let number = whole / iplace;
            whole -= number * iplace;
            out.push((b'0' + number as u8) as char);
        } else if got_one {
            out.push('0');
        }
        iplace /= 10;
    }
    if !got_one {
        out.push('0');
    }
    if precision > 0 {
        out.push('.');
        let mut fplace = fplace_full / 10;
        for _ in 0..precision {
            let number = fraction / fplace;
            fraction -= number * fplace;
            out.push((b'0' + number as u8) as char);
            fplace /= 10;
        }
    }
}

/// C `sprintf("%.*f", precision, val)`, with glibc NaN/Inf spellings.
fn fmt_fixed_c(val: f64, precision: usize) -> String {
    if val.is_nan() {
        return "nan".to_string();
    }
    if val.is_infinite() {
        return if val < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    format!("{val:.precision$}")
}

/// C `sprintf("%*.*e", width, precision, val)`: scientific notation with a
/// signed, ≥2-digit exponent, right-justified in `width`. glibc NaN/Inf
/// spellings.
fn fmt_exp_c(val: f64, precision: usize, width: usize) -> String {
    let body = if val.is_nan() {
        "nan".to_string()
    } else if val.is_infinite() {
        if val < 0.0 { "-inf" } else { "inf" }.to_string()
    } else {
        // Rust `{:.*e}` rounds the mantissa half-to-even (matching glibc)
        // and emits `<mantissa>e<exp>` with no exponent sign / leading
        // zero; reformat the exponent to C's `e±dd`.
        let raw = format!("{val:.precision$e}");
        let (mantissa, exp) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
        let exp_num: i32 = exp.parse().unwrap_or(0);
        let sign = if exp_num < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp_num.abs())
    };
    // Right-justify in `width` (C `%*.*e` pads with leading spaces).
    format!("{body:>width$}")
}

/// RISC alignment padding bytes required between metadata header and value.
pub(crate) fn sts_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // status(2)+severity(2) = 4 → short/enum need 0 pad.
        // `dbr_sts_char` (db_access.h:218-223): `dbr_char_t RISC_pad`
        // (epicsInt8, 1 byte) between severity and value. DBF_UCHAR promotes
        // to DBR_CHAR (db_convert.h dbDBRnewToDBRold) so it shares the pad.
        DbFieldType::Char | DbFieldType::UChar => &[0],
        // `dbr_sts_double` (db_access.h:233-238): `dbr_long_t RISC_pad`
        // (epicsInt32, **4 bytes** — `db_access.h:45 typedef epicsInt32
        // dbr_long_t`) between severity and value. Layout is
        // status(2)+severity(2)+RISC_pad(4)+value(8) → value at offset 8.
        // Int64/UInt64 are served over CA as DBR_DOUBLE so they share the pad;
        // DBF_ULONG promotes to DBR_DOUBLE (db_convert.h dbDBRnewToDBRold) too.
        // DBF_USHORT promotes to DBR_LONG, which has no STS pad, so it stays
        // in the `_` arm with Short/Long/Float.
        DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
            &[0, 0, 0, 0]
        }
        _ => &[],
    }
}

/// RISC alignment padding bytes for TIME structs (after 12-byte metadata header).
pub(crate) fn time_pad(native: DbFieldType) -> &'static [u8] {
    match native {
        // 12 bytes header → short/enum need 2-pad, char needs 2+1=3 pad.
        // DBF_UCHAR promotes to DBR_CHAR so it shares the char pad.
        DbFieldType::Short | DbFieldType::Enum => &[0, 0],
        DbFieldType::Char | DbFieldType::UChar => &[0, 0, 0],
        // double: 12 → pad 4 to reach 16-byte boundary for 8-byte double.
        // DBF_ULONG promotes to DBR_DOUBLE (db_convert.h dbDBRnewToDBRold) so
        // it shares the 4-byte pad; DBF_USHORT promotes to DBR_LONG (4-byte-
        // aligned at offset 12, no pad) and stays in the `_` arm with Long.
        DbFieldType::Double | DbFieldType::ULong => &[0, 0, 0, 0],
        _ => &[],
    }
}

/// Metadata byte count that precedes `value[0]` for a GR (`ctrl=false`)
/// or CTRL (`ctrl=true`) DBR struct. this is the single source
/// of truth shared with [`serialize_gr_ctrl`] / `write_gr_ctrl_meta` — every component below is the exact byte sequence
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
        // UInt64/ULong have no CA GR/CTRL type and reuse the Double layout
        // (DBF_ULONG promotes to DBR_DOUBLE, db_convert.h dbDBRnewToDBRold).
        DbFieldType::Double | DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
            head + 4 + MAX_UNITS_SIZE + n_limits * 8
        }
        // units[8] + n limits (i16).
        DbFieldType::Short => head + MAX_UNITS_SIZE + n_limits * 2,
        // units[8] + n limits (i32). DBF_USHORT promotes to DBR_LONG, so it
        // reuses the Long GR/CTRL layout.
        DbFieldType::Long | DbFieldType::UShort => head + MAX_UNITS_SIZE + n_limits * 4,
        // units[8] + n limits (u8) + RISC_pad(1). DBF_UCHAR promotes to
        // DBR_CHAR (db_convert.h dbDBRnewToDBRold), reusing the Char layout.
        DbFieldType::Char | DbFieldType::UChar => head + MAX_UNITS_SIZE + n_limits + 1,
    }
}

/// Number of metadata bytes that precede `value[0]` for a DBR type —
/// the single size owner mirrored from the serializers in this module.
/// `dbr_buffer_size` derives payload sizing from this so the
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

/// Append the STS metadata prefix (`status`, `severity`, RISC pad).
fn write_sts_meta(dst: &mut Vec<u8>, native: DbFieldType, status: u16, severity: u16) {
    dst.extend_from_slice(&status.to_be_bytes());
    dst.extend_from_slice(&severity.to_be_bytes());
    dst.extend_from_slice(sts_pad(native));
}

/// Append the TIME metadata prefix (STS plus the EPICS timestamp).
fn write_time_meta(
    dst: &mut Vec<u8>,
    native: DbFieldType,
    status: u16,
    severity: u16,
    timestamp: WallTime,
) {
    let (secs, nanos) = epics_timestamp_parts(timestamp);
    dst.extend_from_slice(&status.to_be_bytes());
    dst.extend_from_slice(&severity.to_be_bytes());
    dst.extend_from_slice(&secs.to_be_bytes());
    dst.extend_from_slice(&nanos.to_be_bytes());
    dst.extend_from_slice(time_pad(native));
}

fn serialize_sts(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
) -> CaResult<Vec<u8>> {
    let mut buf = Vec::with_capacity(4 + sts_pad(native).len() + val_bytes.len());
    write_sts_meta(&mut buf, native, status, severity);
    buf.extend_from_slice(val_bytes);
    Ok(buf)
}

fn serialize_time(
    native: DbFieldType,
    val_bytes: &[u8],
    status: u16,
    severity: u16,
    timestamp: WallTime,
) -> CaResult<Vec<u8>> {
    let mut buf = Vec::with_capacity(12 + time_pad(native).len() + val_bytes.len());
    write_time_meta(&mut buf, native, status, severity, timestamp);
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
        // DBF_USHORT promotes to DBR_LONG (db_convert.h dbDBRnewToDBRold), so
        // it serializes with the Long GR/CTRL layout.
        DbFieldType::Long | DbFieldType::UShort => {
            // units[8] + 6 or 8 limits (i32)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits * 4]);
        }
        // DBF_UCHAR promotes to DBR_CHAR (db_convert.h dbDBRnewToDBRold), so
        // it serializes with the Char GR/CTRL layout.
        DbFieldType::Char | DbFieldType::UChar => {
            // units[8] + 6 or 8 limits (u8) + RISC_pad(1)
            buf.extend_from_slice(&[0u8; MAX_UNITS_SIZE]);
            let n_limits = if ctrl { 8 } else { 6 };
            buf.extend_from_slice(&vec![0u8; n_limits]);
            buf.push(0); // RISC_pad
        }
        // Int64/UInt64/ULong have no CA GR/CTRL type — this arm is unreachable
        // in normal CA paths. Use same layout as Double
        // (precision(2)+pad(2)+units(8)+n*f64 limits); DBF_ULONG promotes to
        // DBR_DOUBLE (db_convert.h dbDBRnewToDBRold).
        DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
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
///
/// Thin wrapper over [`encode_dbr_into`] — use that on any path that already
/// owns the buffer the bytes are headed for.
pub fn encode_dbr(
    dbr_type: u16,
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<Vec<u8>> {
    let mut buf = Vec::new();
    encode_dbr_into(&mut buf, dbr_type, snapshot)?;
    Ok(buf)
}

/// Append the DBR wire body for `snapshot` to `dst`, leaving whatever `dst`
/// already holds untouched.
///
/// This is the shape C uses. `read_reply` never builds a standalone payload:
/// `cas_copy_in_header` reserves the header *and* the payload inside the
/// client's existing send buffer and hands back `pPayload`
/// (`rsrv/camessage.c:516`), then `dbGet`'s `!dbfl_has_copy(pfl)` arm converts
/// the record's live field straight into that space (`dbAccess.c:1020`). So the
/// caller reserves its header room in `dst` and the metadata plus the converted
/// value land after it — a 1 MiB waveform is materialised once, not built here
/// and copied into a frame.
///
/// One deviation remains and is deliberate: a request whose DBR type differs
/// from the value's native type still builds a converted `EpicsValue` first
/// (`convert_and_serialize` → `EpicsValue::convert_to`), because the port's
/// conversion is value-to-value where C's `convert()` is field-to-buffer. The
/// native-type case — every monitor on an unconverted field, including the
/// array fan-out this exists for — takes the zero-copy path.
pub fn encode_dbr_into(
    dst: &mut Vec<u8>,
    dbr_type: u16,
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<()> {
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
        dst.extend_from_slice(&buf);
        return Ok(());
    }
    let native = super::native_type_for_dbr(dbr_type)?;
    let status = snapshot.alarm.status;
    let severity = snapshot.alarm.severity;

    // Metadata first — it precedes the value in every DBR layout — so the
    // value can then be converted directly onto the tail of `dst`.
    match dbr_type {
        0..=6 => {}
        7..=13 => write_sts_meta(dst, native, status, severity),
        14..=20 => write_time_meta(dst, native, status, severity, snapshot.timestamp),
        21..=27 => write_gr_meta(dst, native, snapshot),
        28..=34 => write_ctrl_meta(dst, native, snapshot),
        // PUT_ACKT (35) and PUT_ACKS (36) are write-only on the wire:
        // the server should never produce them in a read response. We
        // expose a no-op encoding so callers that round-trip through
        // encode_dbr (e.g. forwarders) don't fail loudly.
        super::DBR_PUT_ACKT | super::DBR_PUT_ACKS => {}
        // STSACK_STRING — alarm acknowledge string response. Layout:
        //   status(2) severity(2) ackt(2) acks(2) value(40) = 48 bytes.
        // ackt/acks are taken from snapshot.alarm if available; the
        // value itself comes from the encoded native String.
        super::DBR_STSACK_STRING => {
            let ackt = snapshot.alarm.ackt.unwrap_or(0);
            let acks = snapshot.alarm.acks.unwrap_or(0);
            dst.extend_from_slice(&status.to_be_bytes());
            dst.extend_from_slice(&severity.to_be_bytes());
            dst.extend_from_slice(&ackt.to_be_bytes());
            dst.extend_from_slice(&acks.to_be_bytes());
        }
        // CLASS_NAME (38) handled by the early-return above.
        _ => return Err(CaError::UnsupportedType(dbr_type)),
    }

    // a `*_STRING` request of a Double/Float field must honor the
    // record's precision (C `getDoubleString` → `cvtDoubleToString`).
    // `EpicsValue::convert_to(String)` (value.rs) has no record context, so
    // route string requests through the precision-aware converter here.
    if native == DbFieldType::String {
        convert_value_to_dbr_string(&snapshot.value, snapshot).write_into(dst);
    } else {
        convert_and_serialize_into(native, &snapshot.value, dst);
    }
    Ok(())
}

/// Append GR (graphic/display) metadata — the real snapshot values, not the
/// zeroed layout `serialize_gr_ctrl` emits.
fn write_gr_meta(
    dst: &mut Vec<u8>,
    native: DbFieldType,
    snapshot: &crate::server::snapshot::Snapshot,
) {
    write_gr_ctrl_meta(dst, native, snapshot, 6)
}

/// Append CTRL (control) metadata. Same as GR but with 8 limits (adds
/// upper/lower ctrl).
fn write_ctrl_meta(
    dst: &mut Vec<u8>,
    native: DbFieldType,
    snapshot: &crate::server::snapshot::Snapshot,
) {
    write_gr_ctrl_meta(dst, native, snapshot, 8)
}

/// The one GR/CTRL metadata layout owner; `n_limits` (6 vs 8) is the only
/// difference between the two DBR classes.
fn write_gr_ctrl_meta(
    dst: &mut Vec<u8>,
    native: DbFieldType,
    snapshot: &crate::server::snapshot::Snapshot,
    n_limits: usize,
) {
    dst.extend_from_slice(&snapshot.alarm.status.to_be_bytes());
    dst.extend_from_slice(&snapshot.alarm.severity.to_be_bytes());

    match native {
        DbFieldType::String => {
            dst.extend_from_slice(sts_pad(native));
        }
        DbFieldType::Enum => {
            encode_enum_metadata(dst, snapshot);
        }
        DbFieldType::Float => {
            encode_prec_units_limits_f32(dst, snapshot, n_limits);
        }
        DbFieldType::Double => {
            encode_prec_units_limits_f64(dst, snapshot, n_limits);
        }
        DbFieldType::Short => {
            encode_units_limits_i16(dst, snapshot, n_limits);
        }
        // DBF_USHORT promotes to DBR_LONG; use the Long GR/CTRL layout.
        DbFieldType::Long | DbFieldType::UShort => {
            encode_units_limits_i32(dst, snapshot, n_limits);
        }
        // DBF_UCHAR promotes to DBR_CHAR; use the Char GR/CTRL layout.
        DbFieldType::Char | DbFieldType::UChar => {
            encode_units_limits_u8(dst, snapshot, n_limits);
            dst.push(0); // RISC_pad
        }
        // Int64/UInt64/ULong have no CA GR/CTRL type; use Double layout
        // (DBF_ULONG promotes to DBR_DOUBLE).
        DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
            encode_prec_units_limits_f64(dst, snapshot, n_limits);
        }
    }
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

/// Decode a NUL-terminated fixed-width CA string field into a lossy `String`.
/// Used for metadata **labels** (units, enum-state strings) that are internal
/// ASCII-grammar identifiers, not arbitrary wire values — lossy decoding is
/// the documented policy for labels. For string **values** use
/// [`read_pv_string`], which preserves the raw bytes.
/// Decode a NUL-terminated fixed-width CA string field into a byte-preserving
/// [`PvString`]. CA `DBR_STRING` slots are historically Latin-1 / arbitrary
/// bytes; preserving them verbatim (no UTF-8 validation) matches pvxs and
/// keeps a gateway pass-through lossless.
fn read_pv_string(data: &[u8], off: usize, max_len: usize) -> PvString {
    let end = data.len().min(off + max_len);
    if off >= end {
        return PvString::new();
    }
    let slice = &data[off..end];
    let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    PvString::from_bytes(&slice[..nul])
}

fn epics_secs_to_wall_time(secs: u32, nanos: u32) -> WallTime {
    let unix_secs = secs as u64 + EPICS_UNIX_EPOCH_OFFSET_SECS;
    // Build from the exact wire integers: a `SystemTime` would round `nanos`
    // to 100 ns on Windows, dropping the low nsec a CA TIME response carries.
    WallTime::from_unix(unix_secs, nanos)
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
        let name = read_pv_string(data, 0, 40);
        let mut snap = Snapshot::new(
            EpicsValue::String(name.clone()),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        );
        snap.class_name = Some(name.as_str_lossy().into_owned());
        return Ok(snap);
    }
    // DBR_STSACK_STRING (37): alarm-acknowledge string. Layout per
    // `dbr_stsack_string` (db_access.h:184-190):
    //   status(2) severity(2) ackt(2) acks(2) value(40) = 48 bytes.
    // The inverse of the `encode_dbr` STSACK_STRING arm — without
    // this the encode/decode pair is asymmetric.
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
        let value = read_pv_string(data, 8, 40);
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
    // Guard a truncated metadata-prefixed payload before any decode_*
    // helper slices `&data[meta..]`. `dbr_meta_size` is the exact count
    // of metadata bytes preceding `value[0]` for this (type, native) — it
    // equals the offset `decode_sts` / `decode_time` / `decode_gr_ctrl`
    // advance to (including the latter's unconditional `off`-advancing
    // loops), so this single check makes every value-slice site in-bounds.
    // A short frame is a malformed/hostile CA payload, not a partial
    // result: return `Protocol` rather than panicking with a
    // slice-out-of-bounds (a remotely-triggerable client DoS on the
    // monitor and GET-metadata decode paths). C casts the struct unchecked
    // (heap over-read UB); we reject. For plain values (0..=6) the size is
    // 0, so this is a no-op and `from_bytes_array` handles its own sizing.
    let meta = dbr_meta_size(dbr_type, native);
    if data.len() < meta {
        return Err(CaError::Protocol(format!(
            "DBR type {dbr_type} requires at least {meta} metadata bytes, got {}",
            data.len()
        )));
    }
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
    let timestamp = epics_secs_to_wall_time(secs, nanos);
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
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
        // DBF_USHORT promotes to DBR_LONG; decode with the Long GR/CTRL layout.
        DbFieldType::Long | DbFieldType::UShort => {
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
        // DBF_UCHAR promotes to DBR_CHAR over CA (identical GR/CTRL wire
        // form: 1-byte limits), so it decodes with the Char layout.
        DbFieldType::Char | DbFieldType::UChar => {
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
        // Int64/UInt64/ULong have no CA GR/CTRL type; decode as Double layout
        // (DBF_ULONG promotes to DBR_DOUBLE).
        DbFieldType::Int64 | DbFieldType::UInt64 | DbFieldType::ULong => {
            let precision = read_i16(data, off)?;
            off += 4; // precision(2) + pad(2)
            let units = read_pv_string(data, off, MAX_UNITS_SIZE);
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
        let s = read_pv_string(data, pos, MAX_ENUM_STRING_SIZE);
        if i < no_str {
            strings.push(s);
        }
        pos += MAX_ENUM_STRING_SIZE;
    }
    // Decoding a server's `DBR_GR_ENUM` reply: the `no_str` labels it sent are
    // all a CLIENT has, so they are both its label list and its string table.
    Ok((EnumInfo::new(strings), pos))
}

#[cfg(test)]
mod wire_format_tests {
    use super::*;
    use crate::types::dbr::{
        DBR_CTRL_CHAR, DBR_CTRL_DOUBLE, DBR_DOUBLE, DBR_GR_DOUBLE, DBR_GR_ENUM, DBR_STS_CHAR,
        DBR_STS_DOUBLE, DBR_TIME_DOUBLE, dbr_buffer_size, native_type_for_dbr,
    };

    /// A truncated metadata-prefixed DBR payload must return `Err`, never
    /// panic with a slice-out-of-bounds. Pre-fix `decode_sts` /
    /// `decode_time` / `decode_gr_ctrl` sliced `&data[meta..]` after
    /// proving only `data.len() >= 4`/`>= 12`, so a short frame from the
    /// monitor / GET-metadata paths panicked — a remotely-triggerable
    /// client DoS. Boundary per metadata category: exactly
    /// `dbr_meta_size - 1` bytes is the largest payload that still must be
    /// rejected; a full metadata payload still decodes.
    #[test]
    fn decode_dbr_truncated_metadata_rejected_not_panic() {
        // One representative per metadata category (STS pad, TIME pad,
        // GR enum-string region, CTRL char limits) plus the cited
        // STS_DOUBLE / STS_CHAR / TIME_DOUBLE cases.
        for dbr_type in [
            DBR_STS_DOUBLE,
            DBR_STS_CHAR,
            DBR_TIME_DOUBLE,
            DBR_GR_ENUM,
            DBR_CTRL_CHAR,
        ] {
            let native = native_type_for_dbr(dbr_type).unwrap();
            let meta = dbr_meta_size(dbr_type, native);
            assert!(meta >= 1, "metadata types have a non-zero prefix");

            // Every length in [0, meta) must be rejected (not panic).
            for len in [0usize, meta - 1] {
                let truncated = vec![0u8; len];
                let r = decode_dbr(dbr_type, &truncated, 1);
                assert!(
                    matches!(r, Err(CaError::Protocol(_))),
                    "dbr_type {dbr_type}: {len}-byte payload (meta={meta}) must be Protocol Err, got {r:?}"
                );
            }

            // A full metadata + value payload still decodes.
            let full = vec![0u8; meta + 64];
            assert!(
                decode_dbr(dbr_type, &full, 1).is_ok(),
                "dbr_type {dbr_type}: full meta+value payload must decode"
            );
        }
    }

    /// Structural invariant: the encoded DBR length must equal
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

    /// `DBR_STS_DOUBLE` (type 13) wire layout is
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

    /// STS_DOUBLE encode→decode round-trips with the 4-byte pad.
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

    /// Cross-check: STS_CHAR keeps its 1-byte `RISC_pad`
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

    /// `DBR_STSACK_STRING` (37) must decode, not just encode.
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

#[cfg(test)]
mod r57_string_precision_tests {
    //! Numeric→DBR_STRING must match C `cvtDoubleToString` /
    //! `cvtFloatToString` (libCom cvtFast.c), including the round-half-up
    //! fast path and the `%.*f` / `%*.*e` overflow fallbacks.
    use super::{cvt_double_to_string, cvt_float_to_string};

    #[test]
    fn double_fixed_point_applies_precision() {
        assert_eq!(cvt_double_to_string(3.14, 3), "3.140");
        assert_eq!(cvt_double_to_string(1.0, 3), "1.000");
        assert_eq!(cvt_double_to_string(1.0, 0), "1");
        assert_eq!(cvt_double_to_string(0.0, 3), "0.000");
        assert_eq!(cvt_double_to_string(-3.14159, 2), "-3.14");
        assert_eq!(cvt_double_to_string(123.456, 2), "123.46");
        assert_eq!(cvt_double_to_string(123.456, 0), "123");
    }

    #[test]
    fn double_fast_path_rounds_half_up() {
        // C cvtFast.c uses `(fraction + 5) / 10` — round half *up*, unlike
        // Rust's default round-half-to-even.
        assert_eq!(cvt_double_to_string(0.125, 2), "0.13");
        assert_eq!(cvt_double_to_string(2.5, 0), "3");
        assert_eq!(cvt_double_to_string(0.5, 0), "1");
    }

    #[test]
    fn double_exp_path_for_huge_or_highprec() {
        // |val| > 1e16 → "%*.*e" with width = precision + 7.
        assert_eq!(cvt_double_to_string(1e20, 6), " 1.000000e+20");
        // precision > 8 → exponential, precision clamped to 17.
        assert_eq!(cvt_double_to_string(3.14159, 10), " 3.1415900000e+00");
    }

    #[test]
    fn double_mid_range_uses_fixed_with_clamped_prec() {
        // 1e7 < |val| <= 1e16 → "%.*f" with precision clamped to 3.
        assert_eq!(cvt_double_to_string(5e7, 0), "50000000");
    }

    #[test]
    fn double_nan_inf_glibc_spelling() {
        // NaN takes the `%.*f` branch (NaN compares false against the e-path
        // thresholds) → unpadded glibc "nan".
        assert_eq!(cvt_double_to_string(f64::NAN, 3), "nan");
        // ±Inf exceeds 1e16 → "%*.*e" branch: glibc spelling right-justified
        // in width = precision + 7 (= 13 here), matching C `cvtDoubleToString`.
        assert_eq!(cvt_double_to_string(f64::INFINITY, 6), "          inf");
        assert_eq!(cvt_double_to_string(f64::NEG_INFINITY, 6), "         -inf");
        assert_eq!(cvt_double_to_string(f64::INFINITY, 6).trim(), "inf");
    }

    #[test]
    fn float_fixed_point_applies_precision() {
        assert_eq!(cvt_float_to_string(3.5_f32, 2), "3.50");
        assert_eq!(cvt_float_to_string(1.0_f32, 3), "1.000");
        assert_eq!(cvt_float_to_string(-2.0_f32, 1), "-2.0");
    }
}

#[cfg(test)]
mod r58_enum_label_tests {
    //! An enum value requested as a `*_STRING` DBR must render the
    //! state label (C `getEnumString` → `get_enum_str`), not the index.
    use super::{EpicsValue, convert_value_to_dbr_string};
    use crate::server::snapshot::{EnumInfo, Snapshot};
    use std::time::SystemTime;

    fn snap_with_enum(value: EpicsValue, labels: &[&str]) -> Snapshot {
        let mut s = Snapshot::new(value, 0, 0, SystemTime::UNIX_EPOCH);
        s.enums = Some(EnumInfo::new(labels.iter().map(|x| (*x).into()).collect()));
        s
    }

    #[test]
    fn enum_renders_label_not_index() {
        let s = snap_with_enum(EpicsValue::Enum(1), &["Off", "On"]);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("On".into())
        );
        let s0 = snap_with_enum(EpicsValue::Enum(0), &["Off", "On"]);
        assert_eq!(
            convert_value_to_dbr_string(&s0.value, &s0),
            EpicsValue::String("Off".into())
        );
    }

    #[test]
    fn enum_array_renders_labels() {
        let s = snap_with_enum(EpicsValue::EnumArray(vec![0, 1, 0]), &["Off", "On"]);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::StringArray(vec!["Off".into(), "On".into(), "Off".into()])
        );
    }

    /// CORRECTED (was `enum_out_of_range_falls_back_to_index`, asserting
    /// `"5"`). No C conversion row renders an enum as its number. A channel
    /// whose string table is a plain label list has no out-of-range sentinel —
    /// C `getMenuString` fails the `dbGet` with `S_db_badChoice` — so the port
    /// answers empty. Ground truth, compiled `softIoc`: an `mbbi` put to an
    /// index with no state string answers `caget -t` with an empty line.
    #[test]
    fn enum_out_of_range_renders_the_overflow_never_the_index() {
        let s = snap_with_enum(EpicsValue::Enum(5), &["Off", "On"]);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("".into())
        );
    }

    /// The record sentinel reaches the wire when the field HAS one: `mbbi`'s
    /// `get_enum_str` answers `"Illegal Value"` past state 15
    /// (`mbbiRecord.c:252`). Measured: `caput E:MBBI.VAL 20` then
    /// `caget -t E:MBBI` prints `Illegal Value`.
    #[test]
    fn enum_past_the_slots_renders_the_records_sentinel() {
        use crate::server::snapshot::EnumStringForm;
        let mut s = Snapshot::new(EpicsValue::Enum(20), 0, 0, SystemTime::UNIX_EPOCH);
        let mut slots: Vec<crate::types::PvString> = vec!["zero".into(), "one".into()];
        slots.resize(16, crate::types::PvString::new());
        s.enums = Some(EnumInfo::with_string_form(
            vec!["zero".into(), "one".into()],
            EnumStringForm::states(slots, "Illegal Value".into()),
        ));
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("Illegal Value".into())
        );
    }

    /// An UNDEFINED state inside the slot range is the empty string, not the
    /// sentinel and not the index: C `strncpy`s the empty state
    /// (`mbbiRecord.c:246-250`). Measured: `caput E:MBBI.VAL 5` -> `caget -t`
    /// prints an empty line, while `caget -t -n` still prints `5`.
    #[test]
    fn enum_undefined_slot_renders_empty() {
        use crate::server::snapshot::EnumStringForm;
        let mut s = Snapshot::new(EpicsValue::Enum(5), 0, 0, SystemTime::UNIX_EPOCH);
        let mut slots: Vec<crate::types::PvString> = vec!["zero".into(), "one".into()];
        slots.resize(16, crate::types::PvString::new());
        s.enums = Some(EnumInfo::with_string_form(
            vec!["zero".into(), "one".into()],
            EnumStringForm::states(slots, "Illegal Value".into()),
        ));
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("".into())
        );
    }

    /// CORRECTED (was `enum_without_metadata_falls_back_to_index`, asserting
    /// `"1"`). A channel with no enum source at all is `S_db_noRSET` in C — an
    /// ERROR, never the number. Empty is the closest the port can answer
    /// without inventing a string no C IOC emits.
    #[test]
    fn enum_without_metadata_renders_empty_never_the_index() {
        let s = Snapshot::new(EpicsValue::Enum(1), 0, 0, SystemTime::UNIX_EPOCH);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("".into())
        );
    }
}

#[cfg(test)]
mod r17_1_negative_precision_tests {
    //! R17-1: a NEGATIVE `PREC` is REINTERPRETED as `epicsUInt16`, never
    //! clamped. C `getDoubleString` (dbConvert.c:786) passes the `long
    //! precision` its `get_precision` RSET returned straight into
    //! `cvtDoubleToString(double, char*, epicsUInt16 precision)`, and the
    //! implicit conversion makes `PREC=-1` a precision of 65535 — the
    //! `precision > 8` branch (cvtFast.c:111), capped at 17, `%*.*e`.
    //!
    //! Ground truth, compiled libCom (`cvtDoubleToString(3.7, b, (short)-1)`):
    //! ` 3.70000000000000018e+00`; `cvtFloatToString(3.7f, b, (short)-1)`:
    //! `3.700000047684e+00` (its own cap is 12).
    use super::{EpicsValue, convert_value_to_dbr_string};
    use crate::server::snapshot::{DisplayInfo, Snapshot};
    use std::time::SystemTime;

    fn snap_with_prec(value: EpicsValue, precision: i16) -> Snapshot {
        let mut s = Snapshot::new(value, 0, 0, SystemTime::UNIX_EPOCH);
        s.display = Some(DisplayInfo {
            precision,
            ..Default::default()
        });
        s
    }

    #[test]
    fn double_with_negative_prec_renders_seventeen_digit_exponential() {
        let s = snap_with_prec(EpicsValue::Double(3.7), -1);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String(" 3.70000000000000018e+00".into()),
            "PREC=-1 is epicsUInt16 65535, not a clamp to 0"
        );
    }

    #[test]
    fn float_with_negative_prec_renders_twelve_digit_exponential() {
        let s = snap_with_prec(EpicsValue::Float(3.7), -1);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("3.700000047684e+00".into()),
            "cvtFloatToString caps the reinterpreted precision at 12"
        );
    }

    #[test]
    fn non_negative_prec_is_unchanged() {
        let s = snap_with_prec(EpicsValue::Double(3.7), 2);
        assert_eq!(
            convert_value_to_dbr_string(&s.value, &s),
            EpicsValue::String("3.70".into())
        );
    }
}
