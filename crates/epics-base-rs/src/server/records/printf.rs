use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// printf record (EPICS 7).
// Evaluates FMT as a printf format string with up to 10 inputs (INP0-INP9, values A-J).
// Each format specifier in FMT is matched to the corresponding input in order.
// VAL holds the resulting string (up to 256 chars).
pub struct PrintfRecord {
    pub val: String,
    pub sizv: u16,
    pub fmt: String,
    // INP0-INP9: input link strings
    pub inp_links: [String; 10],
    // A-J: current numeric values from input links
    pub num_vals: [f64; 10],
}

impl Default for PrintfRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            sizv: 256,
            fmt: String::new(),
            inp_links: Default::default(),
            num_vals: [0.0; 10],
        }
    }
}

impl PrintfRecord {
    fn apply_fmt(&self) -> String {
        let mut result = String::new();
        let bytes = self.fmt.as_bytes();
        let mut i = 0;
        let mut inp_idx = 0;

        while i < bytes.len() {
            if bytes[i] != b'%' {
                result.push(bytes[i] as char);
                i += 1;
                continue;
            }
            // Check %% escape
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                result.push('%');
                i += 2;
                continue;
            }
            // Parse %[flags][width][.prec]type
            let spec_start = i;
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b' ' | b'0' | b'#') {
                i += 1;
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i >= bytes.len() {
                break;
            }
            let spec = bytes[i];
            i += 1;
            let fmt_str = std::str::from_utf8(&bytes[spec_start..i]).unwrap_or("%s");

            let val = if inp_idx < 10 { self.num_vals[inp_idx] } else { 0.0 };
            inp_idx += 1;

            let substituted = match spec {
                b'd' | b'i' => format_int(fmt_str, val as i64),
                b'u' => format_uint(fmt_str, val as u64),
                b'o' => format_octhex(fmt_str, val as u64, b'o'),
                b'x' => format_octhex(fmt_str, val as u64, b'x'),
                b'X' => format_octhex(fmt_str, val as u64, b'X'),
                b'e' | b'E' | b'f' | b'g' | b'G' => format_float(fmt_str, val),
                b's' => format!("{}", val),
                _ => format!("{}", val),
            };
            result.push_str(&substituted);
        }

        let max = (self.sizv as usize).saturating_sub(1);
        if result.len() > max { result.truncate(max); }
        result
    }

    fn inp_index(name: &str) -> Option<usize> {
        // INP0-INP9
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[2] == b'P' {
            let digit = bytes[3];
            if digit.is_ascii_digit() {
                return Some((digit - b'0') as usize);
            }
        }
        None
    }

    fn val_index(name: &str) -> Option<usize> {
        // A-J
        if name.len() == 1 {
            let c = name.as_bytes()[0];
            if (b'A'..=b'J').contains(&c) {
                return Some((c - b'A') as usize);
            }
        }
        None
    }
}

// Returns (width, prec, left_align, zero_pad).
// zero_pad is true when '0' flag is present (e.g. %08d), unless '-' overrides it.
fn parse_width_prec(inner: &str) -> (usize, usize, bool, bool) {
    let left_align = inner.contains('-');
    let zero_pad = !left_align && {
        // After stripping non-'0' flag chars, check if next char is '0' followed by a digit.
        let after_flags = inner.trim_start_matches(|c: char| matches!(c, '-' | '+' | ' ' | '#'));
        after_flags.starts_with('0')
            && after_flags.as_bytes().get(1).map_or(false, |b| b.is_ascii_digit())
    };
    let s = inner.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
    let (width_str, prec_str) = if let Some(dot) = s.find('.') {
        (&s[..dot], &s[dot + 1..])
    } else {
        (s, "")
    };
    let width: usize = width_str.trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0);
    let prec: usize = prec_str.trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(6);
    (width, prec, left_align, zero_pad)
}

// Apply width padding. Handles left-align, right-align, and zero-fill.
// For zero-fill on signed values the sign is placed before the zeros.
fn pad_string(s: String, width: usize, left_align: bool, zero_pad: bool) -> String {
    if width <= s.len() {
        return s;
    }
    if left_align {
        format!("{:<width$}", s, width = width)
    } else if zero_pad {
        if s.starts_with('-') || s.starts_with('+') {
            format!("{}{:0>width$}", &s[..1], &s[1..], width = width - 1)
        } else {
            format!("{:0>width$}", s, width = width)
        }
    } else {
        format!("{:>width$}", s, width = width)
    }
}

fn format_int(fmt: &str, val: i64) -> String {
    let inner = &fmt[1..fmt.len() - 1];
    let (width, _, left_align, zero_pad) = parse_width_prec(inner);
    pad_string(format!("{}", val), width, left_align, zero_pad)
}

fn format_uint(fmt: &str, val: u64) -> String {
    let inner = &fmt[1..fmt.len() - 1];
    let (width, _, left_align, zero_pad) = parse_width_prec(inner);
    pad_string(format!("{}", val), width, left_align, zero_pad)
}

fn format_octhex(fmt: &str, val: u64, spec: u8) -> String {
    let inner = &fmt[1..fmt.len() - 1];
    let (width, _, left_align, zero_pad) = parse_width_prec(inner);
    let s = match spec {
        b'o' => format!("{:o}", val),
        b'x' => format!("{:x}", val),
        _ => format!("{:X}", val),
    };
    pad_string(s, width, left_align, zero_pad)
}

fn format_float(fmt: &str, val: f64) -> String {
    let bytes = fmt.as_bytes();
    let spec = *bytes.last().unwrap_or(&b'g');
    let inner = &fmt[1..fmt.len() - 1];
    let (width, prec, left_align, zero_pad) = parse_width_prec(inner);

    let s = match spec {
        b'e' => format!("{:.prec$e}", val, prec = prec),
        b'E' => format!("{:.prec$E}", val, prec = prec),
        b'f' => format!("{:.prec$}", val, prec = prec),
        b'g' => format_g_val(val, prec, false),
        b'G' => format_g_val(val, prec, true),
        _ => format!("{:.prec$}", val, prec = prec),
    };
    pad_string(s, width, left_align, zero_pad)
}

fn format_g_val(val: f64, prec: usize, upper: bool) -> String {
    if val == 0.0 {
        return "0".to_string();
    }
    let p = if prec == 0 { 1 } else { prec };
    let exp = val.abs().log10().floor() as i32;

    if exp < -4 || exp >= p as i32 {
        let sig_prec = p.saturating_sub(1);
        let raw = if upper {
            format!("{:.prec$E}", val, prec = sig_prec)
        } else {
            format!("{:.prec$e}", val, prec = sig_prec)
        };
        strip_trailing_zeros_sci(&raw, upper)
    } else {
        let decimal_places = (p as i32 - 1 - exp).max(0) as usize;
        let raw = format!("{:.prec$}", val, prec = decimal_places);
        if raw.contains('.') {
            raw.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            raw
        }
    }
}

fn strip_trailing_zeros_sci(s: &str, upper: bool) -> String {
    let sep = if upper { 'E' } else { 'e' };
    if let Some(pos) = s.find(sep) {
        let mantissa = &s[..pos];
        let exp_part = &s[pos..];
        let trimmed = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        format!("{}{}", trimmed, exp_part)
    } else {
        s.to_string()
    }
}

static PRINTF_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL",  dbf_type: DbFieldType::Char,   read_only: true  },
    FieldDesc { name: "SIZV", dbf_type: DbFieldType::Short,  read_only: false },
    FieldDesc { name: "FMT",  dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP0", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP1", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP2", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP3", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP4", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP5", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP6", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP7", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP8", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INP9", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "A",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "B",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "C",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "D",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "E",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "F",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "G",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "H",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "I",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "J",    dbf_type: DbFieldType::Double, read_only: false },
];

impl Record for PrintfRecord {
    fn record_type(&self) -> &'static str {
        "printf"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        PRINTF_FIELDS
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.val = self.apply_fmt();
        Ok(ProcessOutcome::complete())
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.val.as_bytes().to_vec()))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL"  => Some(EpicsValue::CharArray(self.val.as_bytes().to_vec())),
            "SIZV" => Some(EpicsValue::Short(self.sizv as i16)),
            "FMT"  => Some(EpicsValue::String(self.fmt.clone())),
            _ => {
                if let Some(idx) = Self::inp_index(name) {
                    return Some(EpicsValue::String(self.inp_links[idx].clone()));
                }
                if let Some(idx) = Self::val_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "SIZV" => {
                if let EpicsValue::Short(v) = value { self.sizv = v.max(1) as u16; }
            }
            "FMT" => {
                if let EpicsValue::String(s) = value { self.fmt = s; }
                else { return Err(CaError::TypeMismatch("FMT".into())); }
            }
            _ => {
                if let Some(idx) = Self::inp_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_links[idx] = s;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else if let Some(idx) = Self::val_index(name) {
                    self.num_vals[idx] =
                        value.to_f64().ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INP0", "A"), ("INP1", "B"), ("INP2", "C"), ("INP3", "D"), ("INP4", "E"),
            ("INP5", "F"), ("INP6", "G"), ("INP7", "H"), ("INP8", "I"), ("INP9", "J"),
        ]
    }
}
