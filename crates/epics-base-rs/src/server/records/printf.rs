use crate::error::{CaError, CaResult};
use crate::server::record::{ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

// printf record (EPICS 7).
// Evaluates FMT as a printf format string with up to 10 inputs (INP0-INP9, values A-J).
// Each format specifier in FMT consumes the next input in order. A `*`
// (variable width/precision) also consumes one input. VAL holds the
// resulting string (capped by SIZV).
//
// C `printfRecord.c::doPrintf` reads each input link with the DBR type
// implied by the conversion + length modifier: `%s`/`%ls` read a
// string, the numeric conversions read a number. The Rust framework
// pre-fetches every INPn link into the matching value field A..J; we
// therefore store the raw `EpicsValue` per slot so a `%s` conversion
// can recover the string content instead of stringifying a
// coerced f64.
pub struct PrintfRecord {
    pub val: String,
    pub sizv: u16,
    pub fmt: PvString,
    /// INP0-INP9: input link strings.
    pub inp_links: [String; 10],
    /// A-J: current values from the input links, kept in their native
    /// type. `%s` reads the string form, numeric conversions read the
    /// numeric form.
    pub vals: [EpicsValue; 10],
    /// LEN: byte length of the formatted VAL string INCLUDING the
    /// terminating NUL, recomputed each process cycle. C
    /// `printfRecord.c:322` `prec->len = pval - prec->val` and posted at
    /// :400 (DBF_ULONG, like lsi/lso).
    pub len: u32,
    /// IVLS: the "Invalid Link String" emitted in place of any format
    /// directive whose input link did not resolve this cycle. C
    /// `printfRecord.c:306-307` `flags & F_BADLNK ? prec->ivls : format`;
    /// DBF_STRING `size(16)`, `initial("LNK")`.
    pub ivls: String,
    /// Per-cycle scratch: which of INP0..INP9 produced a value this
    /// cycle (link configured AND fetch succeeded). Set by the
    /// framework via [`Record::set_resolved_input_links`] before
    /// `process()`. `apply_fmt` treats a consumed slot that is not
    /// resolved as a bad link — the framework analogue of C
    /// `RTN_SUCCESS(dbGetLink(...))` / `recGblInitConstantLink` failing.
    resolved: [bool; 10],
}

impl Default for PrintfRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            // Intentional deviation from C `printfRecord.dbd.pod`
            // `field(SIZV,DBF_USHORT){ initial("41") }`: the port defaults to a
            // larger 256-byte result buffer rather than C's 41. Kept by design
            // (a .db that needs C's size sets SIZV explicitly); not a parity
            // bug.
            sizv: 256,
            fmt: PvString::new(),
            inp_links: Default::default(),
            vals: std::array::from_fn(|_| EpicsValue::Double(0.0)),
            len: 0,
            ivls: "LNK".to_string(),
            resolved: [false; 10],
        }
    }
}

/// One parsed printf conversion directive.
struct Directive {
    /// Width; `None` when given as `*` (read from the next input).
    width: Option<usize>,
    star_width: bool,
    /// Precision; `None` when absent, `Some(*)` flag handled separately.
    precision: Option<usize>,
    star_prec: bool,
    left_align: bool,
    zero_pad: bool,
    alt_form: bool, // '#'
    /// Final conversion character.
    conv: u8,
    /// `l`/`ll` long modifier present (selects `%ls` long string).
    long: bool,
    bad: bool,
}

impl PrintfRecord {
    /// Stringify an input value for the `%s` / `%ls` conversion.
    /// Mirrors C reading the link as `DBR_STRING` / `DBR_CHAR`.
    fn val_as_string(v: &EpicsValue) -> String {
        match v {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            EpicsValue::CharArray(bytes) => {
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            }
            EpicsValue::Double(d) => format!("{d}"),
            EpicsValue::Float(f) => format!("{f}"),
            EpicsValue::Long(n) => format!("{n}"),
            EpicsValue::Short(n) => format!("{n}"),
            EpicsValue::Int64(n) => format!("{n}"),
            EpicsValue::Char(c) => format!("{c}"),
            EpicsValue::Enum(e) => format!("{e}"),
            other => format!("{other:?}"),
        }
    }

    fn val_as_f64(v: &EpicsValue) -> f64 {
        v.to_f64().unwrap_or(0.0)
    }

    /// Which INP0..INP9 slots FMT consumes with a plain `%s` (no `l`
    /// modifier) — the directives C reads with `DBR_STRING`
    /// (`printfRecord.c:291`), so an ENUM/MENU source delivers its label.
    ///
    /// Deliberate deviation, stated precisely: this map is computed at
    /// FETCH time under the no-failure slot pairing. When a `*` width
    /// link fails at read time, C's `goto bad_format`
    /// (`printfRecord.c:167-168`) skips the conversion's own `plink++`,
    /// so C's LATER directives consume shifted slots with fresh
    /// format-time reads under their own DBR types — a pairing this
    /// fetch-time request cannot see. `apply_fmt` reproduces the slot
    /// SHIFT itself; only the shifted slot's fetch conversion stays as
    /// requested here. Closing that would need either a corrective
    /// second fetch (a PP link source would process twice — a real
    /// semantic break C does not have) or C's lazy read-during-format
    /// model, which the async-read/sync-format phase split rules out.
    /// The failed-`*` directive itself emits IVLS in both
    /// implementations.
    fn plain_string_slots(&self) -> [bool; 10] {
        let mut out = [false; 10];
        let bytes = self.fmt.as_bytes();
        let mut i = 0;
        let mut slot = 0usize;
        while i < bytes.len() && slot < out.len() {
            if bytes[i] != b'%' {
                i += 1;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'%') {
                i += 2;
                continue;
            }
            let (d, next) = Self::parse_directive(bytes, i);
            i = next;
            if d.bad {
                continue;
            }
            // Consumption order mirrors `apply_fmt`: `*` width, then `*`
            // precision (both numeric), then the conversion's own slot.
            if d.star_width {
                slot += 1;
            }
            if d.star_prec {
                slot += 1;
            }
            if slot >= out.len() {
                break;
            }
            if d.conv == b's' && !d.long {
                out[slot] = true;
            }
            slot += 1;
        }
        out
    }

    /// Parse one `%...` directive starting at `bytes[i]` (which is `%`).
    /// Returns the directive and the index just past the conversion
    /// char. Consumes `*` width/precision values from `star_idx`.
    fn parse_directive(bytes: &[u8], mut i: usize) -> (Directive, usize) {
        let mut d = Directive {
            width: None,
            star_width: false,
            precision: None,
            star_prec: false,
            left_align: false,
            zero_pad: false,
            alt_form: false,
            conv: b's',
            long: false,
            bad: false,
        };
        i += 1; // skip '%'
        // Flags.
        loop {
            match bytes.get(i) {
                Some(b'-') => d.left_align = true,
                Some(b'+') | Some(b' ') => {}
                Some(b'#') => d.alt_form = true,
                Some(b'0') => d.zero_pad = true,
                _ => break,
            }
            i += 1;
        }
        // Width.
        if bytes.get(i) == Some(&b'*') {
            d.star_width = true;
            i += 1;
        } else {
            let mut w = 0usize;
            let mut any = false;
            while let Some(c) = bytes.get(i) {
                if c.is_ascii_digit() {
                    w = w * 10 + (c - b'0') as usize;
                    any = true;
                    i += 1;
                } else {
                    break;
                }
            }
            if any {
                d.width = Some(w);
            }
        }
        // Precision.
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            if bytes.get(i) == Some(&b'*') {
                d.star_prec = true;
                i += 1;
            } else {
                let mut p = 0usize;
                while let Some(c) = bytes.get(i) {
                    if c.is_ascii_digit() {
                        p = p * 10 + (c - b'0') as usize;
                        i += 1;
                    } else {
                        break;
                    }
                }
                d.precision = Some(p);
            }
        }
        // Length modifiers: h, hh, l, ll.
        loop {
            match bytes.get(i) {
                Some(b'h') => {
                    i += 1;
                }
                Some(b'l') => {
                    d.long = true;
                    i += 1;
                }
                _ => break,
            }
        }
        // Conversion character.
        match bytes.get(i) {
            Some(&c) if b"diouxXeEfFgGcs".contains(&c) => {
                d.conv = c;
                i += 1;
            }
            Some(_) => {
                d.bad = true;
                i += 1;
            }
            None => {
                d.bad = true;
            }
        }
        (d, i)
    }

    fn apply_fmt(&self) -> String {
        let mut result = String::new();
        let bytes = self.fmt.as_bytes();
        let mut i = 0;
        let mut inp_idx = 0usize;

        // Consume the next input link slot, advancing the cursor (C
        // `linkn++`). Returns the value only when the slot is in range
        // AND its INPn link actually resolved this cycle; otherwise the
        // consumption is a failed link read (C `dbGetLink` /
        // `recGblInitConstantLink` returning non-success → F_BADLNK).
        let take = |idx: &mut usize| -> Option<&EpicsValue> {
            let cur = *idx;
            *idx += 1;
            if cur < 10 && self.resolved[cur] {
                Some(&self.vals[cur])
            } else {
                None
            }
        };

        while i < bytes.len() {
            if bytes[i] != b'%' {
                result.push(bytes[i] as char);
                i += 1;
                continue;
            }
            // %% escape.
            if bytes.get(i + 1) == Some(&b'%') {
                result.push('%');
                i += 2;
                continue;
            }
            let start = i;
            let (d, next) = Self::parse_directive(bytes, i);
            i = next;
            if d.bad {
                // Bad format directive: C `printfRecord.c:306-307` echoes
                // the literal accumulated directive text (`format`) on
                // F_BADFMT, so FMT "x=%q" yields VAL "x=%q". The bytes
                // `[start..i]` are exactly that directive (parse_directive
                // advances past the bad conversion char). Echo them byte
                // for byte, as the literal-run path above does.
                for &b in &bytes[start..i] {
                    result.push(b as char);
                }
                continue;
            }

            // Track a failed link read across the directive's consumed
            // slots. C sets F_BADLNK on the first failure and emits IVLS
            // for the whole directive (printfRecord.c:306-307).
            let mut bad_link = false;

            // Resolve `*` width / precision from the next input(s). A
            // `*` consumes a link slot even when it fails to resolve.
            let width = if d.star_width {
                match take(&mut inp_idx) {
                    Some(v) => Self::val_as_f64(v) as i64,
                    None => {
                        bad_link = true;
                        0
                    }
                }
            } else {
                d.width.unwrap_or(0) as i64
            };
            let precision = if d.star_prec {
                match take(&mut inp_idx) {
                    Some(v) => (Self::val_as_f64(v) as i64).max(0) as usize,
                    None => {
                        bad_link = true;
                        0
                    }
                }
            } else {
                d.precision.unwrap_or(usize::MAX)
            };
            let (width, left_align) = if width < 0 {
                ((-width) as usize, true)
            } else {
                (width as usize, d.left_align)
            };

            // C `goto bad_format` (printfRecord.c:167-168) jumps out
            // BEFORE consuming the conversion's link when a `*` already
            // failed, so a failed star does NOT consume the conversion's
            // INP slot — the next directive inherits it. Only consume the
            // conversion arg when no star failed; an arg that is exhausted
            // or unresolved is itself a bad link.
            let arg = if bad_link {
                None
            } else {
                let a = take(&mut inp_idx);
                if a.is_none() {
                    bad_link = true;
                }
                a
            };

            if bad_link {
                // F_BADLNK: emit the Invalid Link String once for the
                // whole directive (printfRecord.c:307).
                result.push_str(&self.ivls);
                continue;
            }

            let conv_prec = if precision == usize::MAX {
                6
            } else {
                precision
            };

            let substituted = match d.conv {
                b'd' | b'i' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0);
                    pad_string(format!("{}", v as i64), width, left_align, d.zero_pad)
                }
                b'u' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0);
                    pad_string(
                        format!("{}", v as i64 as u64),
                        width,
                        left_align,
                        d.zero_pad,
                    )
                }
                b'o' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0) as i64 as u64;
                    let s = if d.alt_form && v != 0 {
                        format!("0{v:o}")
                    } else {
                        format!("{v:o}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'x' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0) as i64 as u64;
                    let s = if d.alt_form && v != 0 {
                        format!("0x{v:x}")
                    } else {
                        format!("{v:x}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'X' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0) as i64 as u64;
                    let s = if d.alt_form && v != 0 {
                        format!("0X{v:X}")
                    } else {
                        format!("{v:X}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0);
                    let s = format_float_conv(d.conv, v, conv_prec, d.alt_form);
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'c' => {
                    // %c: the input value as a single character (C reads
                    // DBR_CHAR). Numeric value → its code point.
                    let ch = match arg {
                        Some(EpicsValue::String(s)) => {
                            s.as_str_lossy().chars().next().unwrap_or(' ')
                        }
                        Some(v) => {
                            let code = Self::val_as_f64(v) as u32;
                            char::from_u32(code).unwrap_or('\u{0}')
                        }
                        None => '\u{0}',
                    };
                    pad_string(ch.to_string(), width, left_align, false)
                }
                b's' => {
                    // %s / %ls: print the link's STRING content.
                    let mut s = arg.map(Self::val_as_string).unwrap_or_default();
                    // Precision caps the number of characters printed.
                    if precision != usize::MAX && s.chars().count() > precision {
                        s = s.chars().take(precision).collect();
                    }
                    pad_string(s, width, left_align, false)
                }
                _ => String::new(),
            };
            result.push_str(&substituted);
        }

        let max = (self.sizv as usize).saturating_sub(1);
        if result.len() > max {
            // Truncate on a UTF-8 boundary.
            let trunc = (0..=max)
                .rev()
                .find(|&n| result.is_char_boundary(n))
                .unwrap_or(0);
            result.truncate(trunc);
        }
        result
    }

    fn inp_index(name: &str) -> Option<usize> {
        // INP0-INP9
        let bytes = name.as_bytes();
        if bytes.len() == 4 && &bytes[0..3] == b"INP" {
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

// Apply width padding. Handles left-align, right-align, and zero-fill.
// For zero-fill on signed values the sign is placed before the zeros.
fn pad_string(s: String, width: usize, left_align: bool, zero_pad: bool) -> String {
    if width <= s.chars().count() {
        return s;
    }
    if left_align {
        format!("{s:<width$}")
    } else if zero_pad {
        if s.starts_with('-') || s.starts_with('+') {
            format!("{}{:0>width$}", &s[..1], &s[1..], width = width - 1)
        } else {
            format!("{s:0>width$}")
        }
    } else {
        format!("{s:>width$}")
    }
}

fn format_float_conv(conv: u8, val: f64, prec: usize, alt_form: bool) -> String {
    match conv {
        b'e' => format!("{val:.prec$e}"),
        b'E' => format!("{val:.prec$E}"),
        b'f' | b'F' => format!("{val:.prec$}"),
        b'g' => format_g_val(val, prec, false, alt_form),
        b'G' => format_g_val(val, prec, true, alt_form),
        _ => format!("{val:.prec$}"),
    }
}

fn format_g_val(val: f64, prec: usize, upper: bool, alt_form: bool) -> String {
    if val == 0.0 {
        // libc `%g` of 0.0 is "0"; `%#g` keeps trailing zeros.
        if alt_form {
            let p = if prec == 0 { 1 } else { prec };
            let decimals = p.saturating_sub(1);
            return format!("{:.*}", decimals, 0.0);
        }
        return "0".to_string();
    }
    let p = if prec == 0 { 1 } else { prec };
    let exp = val.abs().log10().floor() as i32;

    if exp < -4 || exp >= p as i32 {
        let sig_prec = p.saturating_sub(1);
        let raw = if upper {
            format!("{val:.sig_prec$E}")
        } else {
            format!("{val:.sig_prec$e}")
        };
        if alt_form {
            raw
        } else {
            strip_trailing_zeros_sci(&raw, upper)
        }
    } else {
        let decimal_places = (p as i32 - 1 - exp).max(0) as usize;
        let raw = format!("{val:.decimal_places$}");
        if alt_form {
            raw
        } else if raw.contains('.') {
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
        format!("{trimmed}{exp_part}")
    } else {
        s.to_string()
    }
}

impl Record for PrintfRecord {
    fn record_type(&self) -> &'static str {
        "printf"
    }

    /// printf is THE exception to "a constant link delivers nothing at
    /// process". Its `GET_PRINT` macro (`printfRecord.c:49-52`) is
    ///
    /// ```c
    /// if (dbLinkIsConstant(plink))
    ///     ok = recGblInitConstantLink(plink++, DBRTYPE, &val);
    /// else
    ///     ok = ! dbGetLink(plink++, DBRTYPE, &val, 0, 0);
    /// ```
    ///
    /// — it re-loads the constant on EVERY `doPrintf`, into a local, and never
    /// seeds a value field at init (printf has no A..J storage in C; the port's
    /// A..J are the framework's fetch sink). So its INP0..9 constants must keep
    /// delivering every cycle, and it declares no `constant_init_links`.
    fn constant_inputs_deliver_at_process(&self) -> bool {
        true
    }

    fn long_string_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.val = self.apply_fmt();
        // C `printfRecord.c:321-322`: `*pval++ = 0; prec->len = pval - prec->val`
        // — LEN counts the formatted bytes plus the terminating NUL.
        self.len = (self.val.len() + 1) as u32;
        Ok(ProcessOutcome::complete())
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.val.as_bytes().to_vec()))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::CharArray(self.val.as_bytes().to_vec())),
            "LEN" => Some(EpicsValue::ULong(self.len)),
            "SIZV" => Some(EpicsValue::UShort(self.sizv)),
            "FMT" => Some(EpicsValue::String(self.fmt.clone())),
            "IVLS" => Some(EpicsValue::String(self.ivls.clone().into())),
            _ => {
                if let Some(idx) = Self::inp_index(name) {
                    return Some(EpicsValue::String(self.inp_links[idx].clone().into()));
                }
                if let Some(idx) = Self::val_index(name) {
                    return Some(self.vals[idx].clone());
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "SIZV" => {
                // SIZV is DBF_USHORT (printfRecord.dbd.pod:202): a client put
                // arrives as UShort, internal callers may still pass Short.
                let raw = match value {
                    EpicsValue::UShort(v) => Some(v as i32),
                    EpicsValue::Short(v) => Some(v as i32),
                    _ => None,
                };
                if let Some(raw) = raw {
                    // C printfRecord.c:337-342 clamps SIZV to [16, 0x7fff].
                    self.sizv = raw.clamp(16, 0x7fff) as u16;
                }
            }
            "FMT" => {
                if let EpicsValue::String(s) = value {
                    self.fmt = s;
                } else {
                    return Err(CaError::TypeMismatch("FMT".into()));
                }
            }
            "IVLS" => {
                if let EpicsValue::String(s) = value {
                    // C dbd `field(IVLS,DBF_STRING) size(16)`: a 16-byte
                    // buffer holds at most 15 chars plus the NUL.
                    self.ivls = s.as_str_lossy().chars().take(15).collect();
                } else {
                    return Err(CaError::TypeMismatch("IVLS".into()));
                }
            }
            _ => {
                if let Some(idx) = Self::inp_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_links[idx] = s.as_str_lossy().into_owned();
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else if let Some(idx) = Self::val_index(name) {
                    // Store the raw value so `%s` can recover the
                    // string form of a string-typed input link.
                    self.vals[idx] = value;
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    /// A..J are C locals (`GET_PRINT`'s per-directive `val` buffer,
    /// `printfRecord.c:47`), not DBF-typed fields: each cycle stores
    /// whatever type the directive's request delivered — `%s` a string,
    /// numerics a number. The default's coercion types the slot by its
    /// PREVIOUS value (initially `Double`), which turned every string
    /// delivery into `atof()` = 0.0 before `%s` could read it.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if let Some(idx) = Self::val_index(name) {
            self.vals[idx] = value;
            return Ok(());
        }
        crate::server::record::put_field_internal_default(self, name, value)
    }

    /// C `GET_PRINT` reads each link with the conversion's own `dbrType`:
    /// `%s` is `dbGetLink(..., DBR_STRING, ...)` (`printfRecord.c:291`), so
    /// an ENUM/MENU source delivers its state label, not the index
    /// (epics-base#183). Every other conversion's numeric request is
    /// value-equivalent to the native fetch (`%ls` reads the char array the
    /// native fetch already delivers).
    fn input_link_read_as(
        &self,
        link_field: &str,
        _source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::LinkReadAs;
        Some(match Self::inp_index(link_field) {
            Some(idx) if self.plain_string_slots()[idx] => LinkReadAs::String,
            _ => LinkReadAs::Native,
        })
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INP0", "A"),
            ("INP1", "B"),
            ("INP2", "C"),
            ("INP3", "D"),
            ("INP4", "E"),
            ("INP5", "F"),
            ("INP6", "G"),
            ("INP7", "H"),
            ("INP8", "I"),
            ("INP9", "J"),
        ]
    }

    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        // Record which INPn links produced a value this cycle so
        // `apply_fmt` can emit IVLS for the directives whose link read
        // failed (C `printfRecord.c` F_BADLNK). The framework passes the
        // `link_field` names ("INP0".."INP9") that resolved; any slot not
        // listed is a failed/unconfigured link this cycle.
        self.resolved = [false; 10];
        for &lf in resolved {
            if let Some(idx) = Self::inp_index(lf) {
                self.resolved[idx] = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_with(fmt: &str) -> PrintfRecord {
        PrintfRecord {
            fmt: PvString::from(fmt),
            ..Default::default()
        }
    }

    impl PrintfRecord {
        /// Test helper: simulate INPn resolving to `value` this cycle.
        /// The framework writes the value slot and marks it resolved in
        /// lockstep (processing.rs); a test that only set `vals[idx]`
        /// without this would (correctly) see the slot as a bad link.
        fn set_input(&mut self, idx: usize, value: EpicsValue) {
            self.vals[idx] = value;
            self.resolved[idx] = true;
        }
    }

    /// `%s` prints the input link's STRING content, not a number.
    #[test]
    fn percent_s_formats_string_input() {
        let mut rec = rec_with("name=%s");
        rec.set_input(0, EpicsValue::String("motor1".into()));
        rec.process().unwrap();
        assert_eq!(rec.val, "name=motor1");
    }

    /// a string input survives even when other slots are numeric.
    #[test]
    fn percent_s_with_width_padding() {
        let mut rec = rec_with("[%8s]");
        rec.set_input(0, EpicsValue::String("ab".into()));
        rec.process().unwrap();
        assert_eq!(rec.val, "[      ab]");
    }

    /// `%*d` reads the field width from the next input link.
    #[test]
    fn star_width_consumes_an_input() {
        let mut rec = rec_with("%*d");
        rec.set_input(0, EpicsValue::Long(6)); // width
        rec.set_input(1, EpicsValue::Long(42)); // value
        rec.process().unwrap();
        assert_eq!(rec.val, "    42");
    }

    /// `%ld` long modifier is consumed, not emitted literally.
    #[test]
    fn long_modifier_consumed() {
        let mut rec = rec_with("%ld");
        rec.set_input(0, EpicsValue::Long(99));
        rec.process().unwrap();
        assert_eq!(rec.val, "99");
    }

    /// `%ls` long-string conversion prints the string input.
    #[test]
    fn long_string_conversion() {
        let mut rec = rec_with("%ls");
        rec.set_input(0, EpicsValue::String("hello".into()));
        rec.process().unwrap();
        assert_eq!(rec.val, "hello");
    }

    /// `%c` prints a single character.
    #[test]
    fn percent_c_formats_char() {
        let mut rec = rec_with("%c");
        rec.set_input(0, EpicsValue::Long(65)); // 'A'
        rec.process().unwrap();
        assert_eq!(rec.val, "A");
    }

    /// A bad conversion echoes the literal directive text (C
    /// `printfRecord.c:306`, F_BADFMT), not an empty string.
    #[test]
    fn bad_format_echoes_directive_text() {
        let mut rec = rec_with("x=%q");
        rec.process().unwrap();
        assert_eq!(rec.val, "x=%q");
    }

    /// A trailing bare `%` (no conversion char) is also a bad directive
    /// and echoes verbatim.
    #[test]
    fn trailing_percent_echoes_verbatim() {
        let mut rec = rec_with("v=%");
        rec.process().unwrap();
        assert_eq!(rec.val, "v=%");
    }

    /// LEN reports the formatted byte count INCLUDING the NUL terminator
    /// (C `printfRecord.c:321-322`), and is exposed via get_field.
    #[test]
    fn len_counts_formatted_bytes_plus_nul() {
        let mut rec = rec_with("name=%s");
        rec.set_input(0, EpicsValue::String("motor1".into()));
        rec.process().unwrap();
        assert_eq!(rec.val, "name=motor1");
        // 11 chars + 1 NUL.
        assert_eq!(rec.len, 12);
        assert_eq!(rec.get_field("LEN"), Some(EpicsValue::ULong(12)));
    }

    /// An empty format still has a NUL, so LEN == 1 (boundary: zero-length
    /// VAL), not 0.
    #[test]
    fn len_of_empty_value_is_one() {
        let mut rec = rec_with("");
        rec.process().unwrap();
        assert_eq!(rec.val, "");
        assert_eq!(rec.len, 1);
        assert_eq!(rec.get_field("LEN"), Some(EpicsValue::ULong(1)));
    }

    /// L-2: `%#g` of zero keeps trailing zeros; plain `%g` is "0".
    #[test]
    fn g_zero_alt_form() {
        let mut rec = rec_with("%g");
        rec.set_input(0, EpicsValue::Double(0.0));
        rec.process().unwrap();
        assert_eq!(rec.val, "0");

        let mut rec = rec_with("%#.3g");
        rec.set_input(0, EpicsValue::Double(0.0));
        rec.process().unwrap();
        assert_eq!(rec.val, "0.00");
    }

    /// `%%` escapes a literal percent.
    #[test]
    fn percent_escape() {
        let mut rec = rec_with("100%%");
        rec.process().unwrap();
        assert_eq!(rec.val, "100%");
    }

    /// A directive whose INPn link did not resolve this cycle emits IVLS
    /// (default "LNK"), not a default zero. C `printfRecord.c:307`
    /// F_BADLNK. Boundary: slot in range (idx < 10) but not resolved.
    #[test]
    fn unresolved_link_emits_ivls() {
        let mut rec = rec_with("v=%d");
        // No set_input → INP0 did not resolve.
        rec.process().unwrap();
        assert_eq!(rec.val, "v=LNK");
    }

    /// Only the unresolved directive emits IVLS; resolved neighbours
    /// format normally. C consumes one INP slot per directive.
    #[test]
    fn mixed_resolved_and_unresolved() {
        let mut rec = rec_with("%d/%d");
        rec.set_input(0, EpicsValue::Long(7)); // INP0 resolves
        // INP1 (slot 1) unconfigured.
        rec.process().unwrap();
        assert_eq!(rec.val, "7/LNK");
    }

    /// More directives than the 10 INP slots: the exhausted slot
    /// (idx >= 10) is a bad link. C `linkn >= PRINTF_NLINKS`.
    #[test]
    fn exhausted_links_emit_ivls() {
        let mut rec = rec_with("%d%d%d%d%d%d%d%d%d%d%d"); // 11 directives
        for i in 0..10 {
            rec.set_input(i, EpicsValue::Long(i as i32));
        }
        rec.process().unwrap();
        // First ten format their slot; the 11th is exhausted → IVLS.
        assert_eq!(rec.val, "0123456789LNK");
    }

    /// IVLS is configurable and is what gets emitted on a bad link.
    #[test]
    fn custom_ivls_is_emitted() {
        let mut rec = rec_with("%d");
        rec.put_field("IVLS", EpicsValue::String("BAD".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, "BAD");
        assert_eq!(
            rec.get_field("IVLS"),
            Some(EpicsValue::String("BAD".into()))
        );
    }

    /// IVLS clamps to the C `size(16)` buffer: 15 chars max.
    #[test]
    fn ivls_clamps_to_fifteen_chars() {
        let mut rec = rec_with("");
        rec.put_field("IVLS", EpicsValue::String("0123456789ABCDEFGHIJ".into()))
            .unwrap();
        assert_eq!(rec.ivls, "0123456789ABCDE");
    }

    /// A failed `*` width link short-circuits BEFORE the conversion arg
    /// is consumed (C `goto bad_format`), so the conversion's INP slot is
    /// inherited by the next directive. FMT "%*d|%d": star width (slot 0)
    /// fails → "LNK"; the `d` of the FIRST directive does NOT consume a
    /// slot, so the SECOND `%d` reads slot 1.
    #[test]
    fn failed_star_width_does_not_consume_conversion_slot() {
        let mut rec = rec_with("%*d|%d");
        // Slot 0 (the star width) unresolved; slot 1 resolves to 42.
        rec.set_input(1, EpicsValue::Long(42));
        rec.process().unwrap();
        assert_eq!(rec.val, "LNK|42");
    }
}
