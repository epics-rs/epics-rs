use crate::error::{PvaError, PvaResult};
use crate::pvdata::*;

// ─── Size encoding ───────────────────────────────────────────────────────────

pub fn write_size(buf: &mut Vec<u8>, size: i32, big_endian: bool) {
    if size == -1 {
        buf.push(0xFF);
    } else if size < 254 {
        buf.push(size as u8);
    } else {
        buf.push(0xFE);
        if big_endian {
            buf.extend_from_slice(&(size as u32).to_be_bytes());
        } else {
            buf.extend_from_slice(&(size as u32).to_le_bytes());
        }
    }
}

pub fn read_size(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<i32> {
    if *pos >= buf.len() {
        return Err(PvaError::Protocol(
            "unexpected end of data reading size".into(),
        ));
    }
    let b = buf[*pos];
    *pos += 1;
    match b {
        0xFF => Ok(-1), // null
        0xFE => {
            if *pos + 4 > buf.len() {
                return Err(PvaError::Protocol(
                    "unexpected end of data reading extended size".into(),
                ));
            }
            let val = if big_endian {
                u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
            } else {
                u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
            };
            *pos += 4;
            Ok(val as i32)
        }
        v => Ok(v as i32),
    }
}

// ─── String encoding ─────────────────────────────────────────────────────────

pub fn write_string(buf: &mut Vec<u8>, s: &str, big_endian: bool) {
    write_size(buf, s.len() as i32, big_endian);
    buf.extend_from_slice(s.as_bytes());
}

pub fn read_string(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<String> {
    let size = read_size(buf, pos, big_endian)?;
    if size < 0 {
        return Ok(String::new()); // null → empty
    }
    let size = size as usize;
    if *pos + size > buf.len() {
        return Err(PvaError::Protocol(format!(
            "string length {size} exceeds buffer at pos {}",
            *pos
        )));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + size])
        .map_err(|e| PvaError::Protocol(format!("invalid UTF-8: {e}")))?;
    *pos += size;
    Ok(s.to_string())
}

// ─── Primitive read/write with endianness ────────────────────────────────────

pub fn write_u8(buf: &mut Vec<u8>, val: u8) {
    buf.push(val);
}

pub fn write_u16(buf: &mut Vec<u8>, val: u16, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn write_u32(buf: &mut Vec<u8>, val: u32, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn write_i32(buf: &mut Vec<u8>, val: i32, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn write_i64(buf: &mut Vec<u8>, val: i64, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn write_f32(buf: &mut Vec<u8>, val: f32, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn write_f64(buf: &mut Vec<u8>, val: f64, big_endian: bool) {
    if big_endian {
        buf.extend_from_slice(&val.to_be_bytes());
    } else {
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

pub fn read_u8(buf: &[u8], pos: &mut usize) -> PvaResult<u8> {
    if *pos >= buf.len() {
        return Err(PvaError::Protocol("unexpected end reading u8".into()));
    }
    let val = buf[*pos];
    *pos += 1;
    Ok(val)
}

pub fn read_u16(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<u16> {
    if *pos + 2 > buf.len() {
        return Err(PvaError::Protocol("unexpected end reading u16".into()));
    }
    let val = if big_endian {
        u16::from_be_bytes([buf[*pos], buf[*pos + 1]])
    } else {
        u16::from_le_bytes([buf[*pos], buf[*pos + 1]])
    };
    *pos += 2;
    Ok(val)
}

pub fn read_u32(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<u32> {
    if *pos + 4 > buf.len() {
        return Err(PvaError::Protocol("unexpected end reading u32".into()));
    }
    let val = if big_endian {
        u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
    } else {
        u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
    };
    *pos += 4;
    Ok(val)
}

pub fn read_i32(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<i32> {
    if *pos + 4 > buf.len() {
        return Err(PvaError::Protocol("unexpected end reading i32".into()));
    }
    let val = if big_endian {
        i32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
    } else {
        i32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]])
    };
    *pos += 4;
    Ok(val)
}

pub fn read_i64(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<i64> {
    if *pos + 8 > buf.len() {
        return Err(PvaError::Protocol("unexpected end reading i64".into()));
    }
    let val = if big_endian {
        i64::from_be_bytes([
            buf[*pos],
            buf[*pos + 1],
            buf[*pos + 2],
            buf[*pos + 3],
            buf[*pos + 4],
            buf[*pos + 5],
            buf[*pos + 6],
            buf[*pos + 7],
        ])
    } else {
        i64::from_le_bytes([
            buf[*pos],
            buf[*pos + 1],
            buf[*pos + 2],
            buf[*pos + 3],
            buf[*pos + 4],
            buf[*pos + 5],
            buf[*pos + 6],
            buf[*pos + 7],
        ])
    };
    *pos += 8;
    Ok(val)
}

pub fn read_u64(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<u64> {
    if *pos + 8 > buf.len() {
        return Err(PvaError::Protocol("unexpected end reading u64".into()));
    }
    let val = if big_endian {
        u64::from_be_bytes([
            buf[*pos],
            buf[*pos + 1],
            buf[*pos + 2],
            buf[*pos + 3],
            buf[*pos + 4],
            buf[*pos + 5],
            buf[*pos + 6],
            buf[*pos + 7],
        ])
    } else {
        u64::from_le_bytes([
            buf[*pos],
            buf[*pos + 1],
            buf[*pos + 2],
            buf[*pos + 3],
            buf[*pos + 4],
            buf[*pos + 5],
            buf[*pos + 6],
            buf[*pos + 7],
        ])
    };
    *pos += 8;
    Ok(val)
}

pub fn read_f32(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<f32> {
    let bits = read_u32(buf, pos, big_endian)?;
    Ok(f32::from_bits(bits))
}

pub fn read_f64(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<f64> {
    let bits = read_u64(buf, pos, big_endian)?;
    Ok(f64::from_bits(bits))
}

// ─── Structure introspection (type description) ──────────────────────────────

/// Type code constants for wire format
const TYPE_STRUCT_FULL: u8 = 0xFD;
const TYPE_NULL: u8 = 0xFF;

/// Write a FieldDesc to the wire
pub fn write_field_desc(buf: &mut Vec<u8>, desc: &FieldDesc, big_endian: bool) {
    match desc {
        FieldDesc::Scalar(st) => {
            buf.push(st.type_code());
        }
        FieldDesc::ScalarArray(st) => {
            buf.push(st.array_type_code());
        }
        FieldDesc::Structure { struct_id, fields } => {
            buf.push(TYPE_STRUCT_FULL);
            write_string(buf, struct_id, big_endian);
            write_size(buf, fields.len() as i32, big_endian);
            for (name, field_desc) in fields {
                write_string(buf, name, big_endian);
                write_field_desc(buf, field_desc, big_endian);
            }
        }
    }
}

/// Read a FieldDesc from the wire
pub fn read_field_desc(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<FieldDesc> {
    let type_code = read_u8(buf, pos)?;

    if type_code == TYPE_NULL {
        return Err(PvaError::Protocol("null field descriptor".into()));
    }

    if type_code == TYPE_STRUCT_FULL {
        let struct_id = read_string(buf, pos, big_endian)?;
        let field_count = read_size(buf, pos, big_endian)?;
        let mut fields = Vec::new();
        for _ in 0..field_count {
            let name = read_string(buf, pos, big_endian)?;
            let desc = read_field_desc(buf, pos, big_endian)?;
            fields.push((name, desc));
        }
        return Ok(FieldDesc::Structure { struct_id, fields });
    }

    // Check for scalar array (bit 3 set)
    if let Some(st) = ScalarType::from_array_type_code(type_code) {
        return Ok(FieldDesc::ScalarArray(st));
    }

    // Plain scalar
    if let Some(st) = ScalarType::from_type_code(type_code) {
        return Ok(FieldDesc::Scalar(st));
    }

    Err(PvaError::UnsupportedType(type_code))
}

// ─── Scalar value serialization ──────────────────────────────────────────────

pub fn write_scalar_value(buf: &mut Vec<u8>, val: &ScalarValue, big_endian: bool) {
    match val {
        ScalarValue::Boolean(v) => buf.push(if *v { 1 } else { 0 }),
        ScalarValue::Byte(v) => buf.push(*v as u8),
        ScalarValue::Short(v) => write_u16(buf, *v as u16, big_endian),
        ScalarValue::Int(v) => write_i32(buf, *v, big_endian),
        ScalarValue::Long(v) => write_i64(buf, *v, big_endian),
        ScalarValue::UByte(v) => buf.push(*v),
        ScalarValue::UShort(v) => write_u16(buf, *v, big_endian),
        ScalarValue::UInt(v) => write_u32(buf, *v, big_endian),
        ScalarValue::ULong(v) => {
            if big_endian {
                buf.extend_from_slice(&v.to_be_bytes());
            } else {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        ScalarValue::Float(v) => write_f32(buf, *v, big_endian),
        ScalarValue::Double(v) => write_f64(buf, *v, big_endian),
        ScalarValue::String(v) => write_string(buf, v, big_endian),
    }
}

pub fn read_scalar_value(
    buf: &[u8],
    pos: &mut usize,
    scalar_type: ScalarType,
    big_endian: bool,
) -> PvaResult<ScalarValue> {
    match scalar_type {
        ScalarType::Boolean => {
            let v = read_u8(buf, pos)?;
            Ok(ScalarValue::Boolean(v != 0))
        }
        ScalarType::Byte => {
            let v = read_u8(buf, pos)?;
            Ok(ScalarValue::Byte(v as i8))
        }
        ScalarType::Short => {
            let v = read_u16(buf, pos, big_endian)?;
            Ok(ScalarValue::Short(v as i16))
        }
        ScalarType::Int => {
            let v = read_i32(buf, pos, big_endian)?;
            Ok(ScalarValue::Int(v))
        }
        ScalarType::Long => {
            let v = read_i64(buf, pos, big_endian)?;
            Ok(ScalarValue::Long(v))
        }
        ScalarType::UByte => {
            let v = read_u8(buf, pos)?;
            Ok(ScalarValue::UByte(v))
        }
        ScalarType::UShort => {
            let v = read_u16(buf, pos, big_endian)?;
            Ok(ScalarValue::UShort(v))
        }
        ScalarType::UInt => {
            let v = read_u32(buf, pos, big_endian)?;
            Ok(ScalarValue::UInt(v))
        }
        ScalarType::ULong => {
            let v = read_u64(buf, pos, big_endian)?;
            Ok(ScalarValue::ULong(v))
        }
        ScalarType::Float => {
            let v = read_f32(buf, pos, big_endian)?;
            Ok(ScalarValue::Float(v))
        }
        ScalarType::Double => {
            let v = read_f64(buf, pos, big_endian)?;
            Ok(ScalarValue::Double(v))
        }
        ScalarType::String => {
            let v = read_string(buf, pos, big_endian)?;
            Ok(ScalarValue::String(v))
        }
    }
}

// ─── PvField (value) serialization ───────────────────────────────────────────

/// Read a PvField value given its type description
pub fn read_pv_field(
    buf: &[u8],
    pos: &mut usize,
    desc: &FieldDesc,
    big_endian: bool,
) -> PvaResult<PvField> {
    match desc {
        FieldDesc::Scalar(st) => {
            let val = read_scalar_value(buf, pos, *st, big_endian)?;
            Ok(PvField::Scalar(val))
        }
        FieldDesc::ScalarArray(st) => {
            let count = read_size(buf, pos, big_endian)?;
            let mut arr = Vec::new();
            for _ in 0..count {
                arr.push(read_scalar_value(buf, pos, *st, big_endian)?);
            }
            Ok(PvField::ScalarArray(arr))
        }
        FieldDesc::Structure { struct_id, fields } => {
            let mut pv_fields = Vec::new();
            for (name, field_desc) in fields {
                let val = read_pv_field(buf, pos, field_desc, big_endian)?;
                pv_fields.push((name.clone(), val));
            }
            Ok(PvField::Structure(PvStructure {
                struct_id: struct_id.clone(),
                fields: pv_fields,
            }))
        }
    }
}

/// Write a PvField value
pub fn write_pv_field(buf: &mut Vec<u8>, field: &PvField, big_endian: bool) {
    match field {
        PvField::Scalar(val) => {
            write_scalar_value(buf, val, big_endian);
        }
        PvField::ScalarArray(arr) => {
            write_size(buf, arr.len() as i32, big_endian);
            for val in arr {
                write_scalar_value(buf, val, big_endian);
            }
        }
        PvField::Structure(s) => {
            for (_, field) in &s.fields {
                write_pv_field(buf, field, big_endian);
            }
        }
    }
}

// ─── BitSet encoding ─────────────────────────────────────────────────────────

pub fn read_bitset(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<Vec<u8>> {
    let size = read_size(buf, pos, big_endian)?;
    if size < 0 {
        return Ok(Vec::new());
    }
    let byte_count = size as usize;
    if *pos + byte_count > buf.len() {
        return Err(PvaError::Protocol("bitset exceeds buffer".into()));
    }
    let bits = buf[*pos..*pos + byte_count].to_vec();
    *pos += byte_count;
    Ok(bits)
}

pub fn write_bitset(buf: &mut Vec<u8>, bits: &[u8], big_endian: bool) {
    write_size(buf, bits.len() as i32, big_endian);
    buf.extend_from_slice(bits);
}

/// Check if bit at given index is set in the bitset
pub fn bitset_get(bits: &[u8], index: usize) -> bool {
    let byte_idx = index / 8;
    let bit_idx = index % 8;
    if byte_idx >= bits.len() {
        return false;
    }
    bits[byte_idx] & (1 << bit_idx) != 0
}

// ─── PV Request structure ────────────────────────────────────────────────────

/// Build a pvRequest structure for "field(value,alarm,timeStamp)"
pub fn build_pv_request(big_endian: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    // pvRequest is a structure with struct_id=""
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    // 1 field: "field"
    write_size(&mut buf, 1, big_endian);
    write_string(&mut buf, "field", big_endian);
    // "field" is a structure with 3 empty sub-structures
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 3, big_endian);
    // value (empty struct)
    write_string(&mut buf, "value", big_endian);
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 0, big_endian);
    // alarm (empty struct)
    write_string(&mut buf, "alarm", big_endian);
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 0, big_endian);
    // timeStamp (empty struct)
    write_string(&mut buf, "timeStamp", big_endian);
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 0, big_endian);
    buf
}

/// Build a minimal pvRequest for put: "field(value)"
pub fn build_pv_request_value_only(big_endian: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 1, big_endian);
    write_string(&mut buf, "field", big_endian);
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 1, big_endian);
    write_string(&mut buf, "value", big_endian);
    buf.push(TYPE_STRUCT_FULL);
    write_string(&mut buf, "", big_endian);
    write_size(&mut buf, 0, big_endian);
    buf
}

// ─── Status encoding ────────────────────────────────────────────────────────

/// Read a PVA status. Returns Ok(()) for success, Err for failure.
pub fn read_status(buf: &[u8], pos: &mut usize, big_endian: bool) -> PvaResult<()> {
    let status_byte = read_u8(buf, pos)?;
    if status_byte == 0xFF {
        // OK, no message
        return Ok(());
    }
    let status_type = status_byte & 0x07;
    let _has_message = (status_byte >> 4) != 0;
    let message = read_string(buf, pos, big_endian)?;
    let _call_tree = read_string(buf, pos, big_endian)?;
    if status_type == 0 {
        Ok(()) // OK with message
    } else {
        Err(PvaError::Protocol(format!(
            "server error (type={status_type}): {message}"
        )))
    }
}

/// Write OK status (0xFF = success, no message)
pub fn write_status_ok(buf: &mut Vec<u8>) {
    buf.push(0xFF);
}

// ─── Read structure value with bitset (partial update) ───────────────────────

/// Read all fields of a structure value (used after GET response)
pub fn read_structure_value_with_bitset(
    buf: &[u8],
    pos: &mut usize,
    desc: &FieldDesc,
    big_endian: bool,
) -> PvaResult<PvField> {
    let bits = read_bitset(buf, pos, big_endian)?;

    match desc {
        FieldDesc::Structure { struct_id, fields } => {
            // If bit 0 is set, entire structure is present
            let full = bitset_get(&bits, 0);
            let mut pv_fields = Vec::new();
            let mut bit_index = 1; // bit 0 is the structure itself
            for (name, field_desc) in fields {
                let should_read = full || is_field_set(&bits, field_desc, bit_index);
                if should_read {
                    let val = read_pv_field(buf, pos, field_desc, big_endian)?;
                    pv_fields.push((name.clone(), val));
                } else {
                    // Field not changed, push a default
                    pv_fields.push((name.clone(), default_pv_field(field_desc)));
                }
                bit_index += count_field_bits(field_desc);
            }
            Ok(PvField::Structure(PvStructure {
                struct_id: struct_id.clone(),
                fields: pv_fields,
            }))
        }
        _ => read_pv_field(buf, pos, desc, big_endian),
    }
}

fn is_field_set(bits: &[u8], desc: &FieldDesc, start_bit: usize) -> bool {
    match desc {
        FieldDesc::Scalar(_) | FieldDesc::ScalarArray(_) => bitset_get(bits, start_bit),
        FieldDesc::Structure { fields, .. } => {
            if bitset_get(bits, start_bit) {
                return true;
            }
            let mut idx = start_bit + 1;
            for (_, fd) in fields {
                if is_field_set(bits, fd, idx) {
                    return true;
                }
                idx += count_field_bits(fd);
            }
            false
        }
    }
}

fn count_field_bits(desc: &FieldDesc) -> usize {
    match desc {
        FieldDesc::Scalar(_) | FieldDesc::ScalarArray(_) => 1,
        FieldDesc::Structure { fields, .. } => {
            let mut count = 1; // the structure itself
            for (_, fd) in fields {
                count += count_field_bits(fd);
            }
            count
        }
    }
}

fn default_pv_field(desc: &FieldDesc) -> PvField {
    match desc {
        FieldDesc::Scalar(st) => PvField::Scalar(default_scalar(*st)),
        FieldDesc::ScalarArray(_) => PvField::ScalarArray(Vec::new()),
        FieldDesc::Structure { struct_id, fields } => {
            let pv_fields = fields
                .iter()
                .map(|(name, fd)| (name.clone(), default_pv_field(fd)))
                .collect();
            PvField::Structure(PvStructure {
                struct_id: struct_id.clone(),
                fields: pv_fields,
            })
        }
    }
}

fn default_scalar(st: ScalarType) -> ScalarValue {
    match st {
        ScalarType::Boolean => ScalarValue::Boolean(false),
        ScalarType::Byte => ScalarValue::Byte(0),
        ScalarType::Short => ScalarValue::Short(0),
        ScalarType::Int => ScalarValue::Int(0),
        ScalarType::Long => ScalarValue::Long(0),
        ScalarType::UByte => ScalarValue::UByte(0),
        ScalarType::UShort => ScalarValue::UShort(0),
        ScalarType::UInt => ScalarValue::UInt(0),
        ScalarType::ULong => ScalarValue::ULong(0),
        ScalarType::Float => ScalarValue::Float(0.0),
        ScalarType::Double => ScalarValue::Double(0.0),
        ScalarType::String => ScalarValue::String(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_roundtrip_small() {
        let mut buf = Vec::new();
        write_size(&mut buf, 42, false);
        assert_eq!(buf.len(), 1);
        let mut pos = 0;
        assert_eq!(read_size(&buf, &mut pos, false).unwrap(), 42);
    }

    #[test]
    fn test_size_roundtrip_large() {
        let mut buf = Vec::new();
        write_size(&mut buf, 1000, false);
        assert_eq!(buf.len(), 5); // 0xFE + 4 bytes
        let mut pos = 0;
        assert_eq!(read_size(&buf, &mut pos, false).unwrap(), 1000);
    }

    #[test]
    fn test_size_null() {
        let mut buf = Vec::new();
        write_size(&mut buf, -1, false);
        assert_eq!(buf, vec![0xFF]);
        let mut pos = 0;
        assert_eq!(read_size(&buf, &mut pos, false).unwrap(), -1);
    }

    #[test]
    fn test_string_roundtrip() {
        for be in [false, true] {
            let mut buf = Vec::new();
            write_string(&mut buf, "hello world", be);
            let mut pos = 0;
            assert_eq!(read_string(&buf, &mut pos, be).unwrap(), "hello world");
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn test_field_desc_roundtrip() {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
            ],
        };
        for be in [false, true] {
            let mut buf = Vec::new();
            write_field_desc(&mut buf, &desc, be);
            let mut pos = 0;
            let desc2 = read_field_desc(&buf, &mut pos, be).unwrap();
            assert_eq!(pos, buf.len());
            // Verify structure
            match &desc2 {
                FieldDesc::Structure { struct_id, fields } => {
                    assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "value");
                }
                _ => panic!("expected structure"),
            }
        }
    }

    #[test]
    fn test_bitset_get() {
        let bits = vec![0b00000101, 0b00000010]; // bits 0, 2, 9
        assert!(bitset_get(&bits, 0));
        assert!(!bitset_get(&bits, 1));
        assert!(bitset_get(&bits, 2));
        assert!(bitset_get(&bits, 9));
        assert!(!bitset_get(&bits, 8));
        assert!(!bitset_get(&bits, 100)); // out of range
    }
}
