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
    /// `l` (and only `l`) present — C's `F_LONG`, which selects the `%ls`
    /// long-string read. `ll` clears it for `F_LONGLONG`, so `%lls` reads
    /// `DBR_STRING` in C exactly as `%s` does.
    long: bool,
    /// The length modifier, as C's `F_CHAR`/`F_SHORT`/`F_LONG`/`F_LONGLONG`
    /// flag pair (`printfRecord.c:38-41`, parsed at `:141-156`). It picks the
    /// DBR type of the link read, and therefore the width the value is
    /// narrowed to before it is ever formatted (`:186-227`).
    length: LengthMod,
    bad: bool,
}

/// C's length-modifier flags (`printfRecord.c:38-41`) as one state.
///
/// They are mutually exclusive in C — `h` after `l` (or a third `h`) sets
/// `F_BADFMT` (`:142-155`) — so one enum says what four flag bits said, and
/// the illegal combinations are unrepresentable rather than rejected twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum LengthMod {
    /// No modifier — C's `DBR_LONG` / `DBR_ULONG` default.
    #[default]
    None,
    /// `hh` — `F_CHAR`, `DBR_CHAR` / `DBR_UCHAR`.
    Char,
    /// `h` — `F_SHORT`, `DBR_SHORT` / `DBR_USHORT` (and `DBR_FLOAT` for the
    /// float conversions, `printfRecord.c:220-222`).
    Short,
    /// `l` — `F_LONG`, which C notes "has no real effect" for the numeric
    /// conversions (`:198`, `:212`); it only selects `%ls`.
    Long,
    /// `ll` — `F_LONGLONG`, `DBR_INT64` / `DBR_UINT64`.
    LongLong,
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

    /// The integer C's `dbGetLink(plink, DBR_<int>, &val)` would deliver.
    ///
    /// C never routes an integer conversion through a double: `GET_PRINT`
    /// (`printfRecord.c:47-56`) reads the link straight into an
    /// `epicsInt8`/`16`/`32`/`64`. Going through `f64` first, as this record
    /// used to, loses the low bits of anything past 2^53 before a single digit
    /// is formatted.
    ///
    /// [`EpicsValue::as_int_i64`] is that integer view, with the same
    /// low-N-bits truncation C's `dbConvert` applies. A float source has no
    /// integer view — C's conversion is the C cast, which truncates toward
    /// zero — and a non-finite one has no defined cast, so it lands on 0.
    fn val_as_int(v: &EpicsValue) -> i64 {
        if let Some(n) = v.as_int_i64() {
            return n;
        }
        let f = v.to_f64().unwrap_or(0.0);
        if f.is_finite() { f as i64 } else { 0 }
    }

    /// Narrow to the SIGNED width the length modifier selects — C's
    /// `DBR_CHAR`/`DBR_SHORT`/`DBR_LONG`/`DBR_INT64` read
    /// (`printfRecord.c:188-201`). `F_LONG` has no effect, which is C's own
    /// comment at `:198`.
    fn narrow_signed(n: i64, length: LengthMod) -> i64 {
        match length {
            LengthMod::Char => n as i8 as i64,
            LengthMod::Short => n as i16 as i64,
            LengthMod::LongLong => n,
            LengthMod::None | LengthMod::Long => n as i32 as i64,
        }
    }

    /// Narrow to the UNSIGNED width the length modifier selects — C's
    /// `DBR_UCHAR`/`DBR_USHORT`/`DBR_ULONG`/`DBR_UINT64` read
    /// (`printfRecord.c:203-216`). The two's-complement reinterpretation of a
    /// negative input is what C's unsigned DBR read produces.
    fn narrow_unsigned(n: i64, length: LengthMod) -> u64 {
        match length {
            LengthMod::Char => n as u8 as u64,
            LengthMod::Short => n as u16 as u64,
            LengthMod::LongLong => n as u64,
            LengthMod::None | LengthMod::Long => n as u32 as u64,
        }
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
            length: LengthMod::None,
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
        // C `printfRecord.c:141-156`, transcribed: `h` promotes
        // none->Short->Char, `l` promotes none->Long->LongLong, and mixing
        // the two families (or a third of either) is `F_BADFMT`.
        loop {
            match bytes.get(i) {
                Some(b'h') => {
                    d.length = match d.length {
                        LengthMod::None => LengthMod::Short,
                        LengthMod::Short => LengthMod::Char,
                        _ => {
                            d.bad = true;
                            d.length
                        }
                    };
                    i += 1;
                }
                Some(b'l') => {
                    d.length = match d.length {
                        LengthMod::None => LengthMod::Long,
                        LengthMod::Long => LengthMod::LongLong,
                        _ => {
                            d.bad = true;
                            d.length
                        }
                    };
                    i += 1;
                }
                _ => break,
            }
        }
        // `F_LONG` alone is what `case 's'` tests (`printfRecord.c:228`).
        d.long = d.length == LengthMod::Long;
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
                    let v = arg.map(Self::val_as_int).unwrap_or(0);
                    let v = Self::narrow_signed(v, d.length);
                    pad_string(format!("{v}"), width, left_align, d.zero_pad)
                }
                b'u' => {
                    let v = arg.map(Self::val_as_int).unwrap_or(0);
                    let v = Self::narrow_unsigned(v, d.length);
                    pad_string(format!("{v}"), width, left_align, d.zero_pad)
                }
                b'o' => {
                    let v = Self::narrow_unsigned(arg.map(Self::val_as_int).unwrap_or(0), d.length);
                    let s = if d.alt_form && v != 0 {
                        format!("0{v:o}")
                    } else {
                        format!("{v:o}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'x' => {
                    let v = Self::narrow_unsigned(arg.map(Self::val_as_int).unwrap_or(0), d.length);
                    let s = if d.alt_form && v != 0 {
                        format!("0x{v:x}")
                    } else {
                        format!("{v:x}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'X' => {
                    let v = Self::narrow_unsigned(arg.map(Self::val_as_int).unwrap_or(0), d.length);
                    let s = if d.alt_form && v != 0 {
                        format!("0X{v:X}")
                    } else {
                        format!("{v:X}")
                    };
                    pad_string(s, width, left_align, d.zero_pad)
                }
                b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                    let v = arg.map(Self::val_as_f64).unwrap_or(0.0);
                    // C `printfRecord.c:220-222`: `h` reads the link as
                    // `DBR_FLOAT`, so the value is rounded to single
                    // precision before it is formatted.
                    let v = if d.length == LengthMod::Short {
                        v as f32 as f64
                    } else {
                        v
                    };
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
    let upper = conv.is_ascii_uppercase();
    if !val.is_finite() {
        // C spells the non-finites in the case of the conversion letter —
        // C99 added `%F` for exactly that — and carries the sign bit of
        // both the infinities and NaN. Hoisted above the dispatch so all
        // six conversions answer identically, which is what
        // `epicsSnprintf` -> `vsnprintf` does (`osdStdio.c:29`).
        let sign = if val.is_sign_negative() { "-" } else { "" };
        let word = if val.is_nan() { "nan" } else { "inf" };
        return if upper {
            format!("{sign}{}", word.to_ascii_uppercase())
        } else {
            format!("{sign}{word}")
        };
    }
    match conv {
        b'e' | b'E' => format_e_val(val, prec, upper, alt_form),
        b'g' => format_g_val(val, prec, false, alt_form),
        b'G' => format_g_val(val, prec, true, alt_form),
        // `%f`/`%F`, and the fallback for a conversion the caller's own
        // match already excludes.
        _ => format_f_val(val, prec, alt_form),
    }
}

/// C `%e` / `%E` of a finite value: Rust writes a bare exponent (`1.5e6`)
/// where C writes it signed and at least two digits (`1.500000e+06`).
///
/// The single owner of that exponent form — `%g`'s style-`e` branch and
/// [`format_g`] both emit through here, so the record and the
/// command-line tools cannot disagree about it.
fn format_e_val(val: f64, prec: usize, upper: bool, alt_form: bool) -> String {
    let sep = if upper { 'E' } else { 'e' };
    let raw = if upper {
        format!("{val:.prec$E}")
    } else {
        format!("{val:.prec$e}")
    };
    let (mantissa, exp_digits) = raw
        .split_once(sep)
        .expect("Rust's LowerExp/UpperExp always writes an exponent");
    let e: i32 = exp_digits
        .parse()
        .expect("...followed by a decimal exponent");
    // `%#e` keeps the decimal point even when precision leaves no digits
    // after it.
    let point = if alt_form && !mantissa.contains('.') {
        "."
    } else {
        ""
    };
    format!(
        "{mantissa}{point}{sep}{}{:02}",
        if e < 0 { '-' } else { '+' },
        e.abs()
    )
}

/// C `%f` / `%F` of a finite value. Identical to Rust's fixed-point
/// formatting except that `%#f` keeps the decimal point at precision 0.
fn format_f_val(val: f64, prec: usize, alt_form: bool) -> String {
    let s = format!("{val:.prec$}");
    if alt_form && !s.contains('.') {
        format!("{s}.")
    } else {
        s
    }
}

/// The exponent C `%g` uses to choose between `%e` and `%f` style: the
/// exponent of the value AFTER rounding to `precision` significant
/// digits, not of the raw value. C99 7.19.6.1p8 defines the decision
/// value X as the exponent of the style-`e` conversion, and that
/// conversion rounds first, so at a rounding boundary the rounded
/// magnitude can carry into the next decade (999999.5 at precision 6
/// rounds to 1000000, exponent 5 -> 6) and flip the style.
pub fn g_decision_exponent(abs: f64, precision: usize) -> i32 {
    let raw_exp = abs.log10().floor() as i32;
    // Scale so the value has `precision` digits before the decimal
    // point, round half-to-even, and read the magnitude back: a carry
    // into a new decade shows up as an incremented exponent.
    let scale = 10f64.powi(precision as i32 - 1 - raw_exp);
    // For magnitudes near the f64 range limits the scale factor
    // overflows to +/-inf (or underflows to 0) and the product's
    // `log10` saturates to a garbage exponent. The rounded magnitude
    // cannot meaningfully differ from the raw one at those scales, so
    // fall back to `raw_exp`.
    if !scale.is_finite() || scale == 0.0 {
        return raw_exp;
    }
    let rounded_scaled = (abs * scale).round();
    if !rounded_scaled.is_finite() || rounded_scaled <= 0.0 {
        return raw_exp;
    }
    raw_exp + (rounded_scaled.log10().floor() as i32 - (precision as i32 - 1))
}

/// C `printf("%.*g", precision, x)`: `%f` or `%e` style per
/// [`g_decision_exponent`], trailing zeros and a bare decimal point
/// stripped, exponent signed and padded to two digits.
///
/// This is NOT the workspace's only `%g`. Three transcriptions of the
/// rule exist and are meant to: `epics-pva-rs` (`pvdata::fmt::format_g`,
/// for `pvget`/`pvinfo`/`pvmonitor`) and `asyn-rs` (`param::format_g`,
/// for `paramVal::report`) each carry their own, because sharing one
/// would force a crate dependency none of the three otherwise needs.
/// What holds them equal is `libc_differential` — the same test
/// exists in all three crates and requires byte equality with glibc's
/// `snprintf("%.*g")` over 56,300 values including NaN, the infinities
/// and subnormals. It is what caught this copy printing `NaN` where
/// glibc prints `nan`, and `asyn-rs` dropping the sign of `-nan`.
pub fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        // -0.0 == 0.0 in Rust and C alike, and C prints the sign:
        // `printf("%g", -0.0)` is "-0".
        return if x.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    if x.is_nan() {
        // glibc carries the NaN sign bit into the text and spells it
        // lowercase for a lowercase conversion; Rust's `{}` writes `NaN`
        // and drops the sign. `libc_differential` pins the difference.
        return if x.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let p = precision.max(1);
    let exp = g_decision_exponent(x.abs(), p);
    // C `%g` uses fixed-point when `precision > exp >= -4`. Compare as
    // i32 so a negative exponent does not wrap through usize.
    if exp >= -4 && exp < p as i32 {
        let digits = (p as i32 - 1 - exp).max(0) as usize;
        let s = format!("{x:.digits$}");
        if !s.contains('.') {
            return s;
        }
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        // Rust `{:e}` precision counts mantissa decimals, so N
        // significant digits is precision N-1. `format_e_val` owns the
        // rest — C's exponent is signed and at least two digits (`e+06`)
        // where Rust writes it bare (`e6`).
        strip_trailing_zeros_sci(&format_e_val(x, p - 1, false, false), false)
    }
}

fn format_g_val(val: f64, prec: usize, upper: bool, alt_form: bool) -> String {
    if val == 0.0 {
        // `-0.0 == 0.0` in Rust and C alike, and C prints the sign, so the
        // zero shortcut has to carry it explicitly. libc `%g` of 0.0 is
        // "0"; `%#g` keeps the trailing zeros and the point.
        let sign = if val.is_sign_negative() { "-" } else { "" };
        if alt_form {
            let p = if prec == 0 { 1 } else { prec };
            let decimals = p.saturating_sub(1);
            return format!("{sign}{}", format_f_val(0.0, decimals, true));
        }
        return format!("{sign}0");
    }
    let p = if prec == 0 { 1 } else { prec };
    // Same style decision as `format_g`; `%G`/`%#g` differ only in how
    // the chosen style is emitted.
    let exp = g_decision_exponent(val.abs(), p);

    if exp < -4 || exp >= p as i32 {
        let sig_prec = p.saturating_sub(1);
        let raw = format_e_val(val, sig_prec, upper, alt_form);
        if alt_form {
            raw
        } else {
            strip_trailing_zeros_sci(&raw, upper)
        }
    } else {
        let decimal_places = (p as i32 - 1 - exp).max(0) as usize;
        let raw = format_f_val(val, decimal_places, alt_form);
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

/// Every assertion in this file whose reference is glibc itself.
///
/// One home and one gate: `libc` is a `cfg(unix)` dependency of this
/// crate, so a glibc differential written anywhere else in the file
/// fails to build on Windows.
///
/// The `%g` half makes three transcriptions of `format_g` safe.
///
/// `epics-base-rs` (`printf` record), `epics-pva-rs` (`pvget`/`pvinfo`/
/// `pvmonitor`) and `asyn-rs` (`paramVal::report`) each carry their own
/// `format_g`, because sharing one would force a crate dependency none of
/// the three otherwise needs. What keeps them equal is not review but this
/// test: each crate runs the SAME sample through its own `format_g` and
/// through glibc's `snprintf("%.*g")` and requires byte equality. A
/// transcription that drifts fails here, in its own crate, on the sample
/// that caught the drift.
///
/// Gated on `target_env = "gnu"`: the assertion is glibc's exact output,
/// and newlib (RTEMS) / musl are not that reference.
#[cfg(all(test, unix, target_env = "gnu"))]
mod libc_differential {
    use super::format_g;

    /// glibc `printf("%.*g", prec, v)`.
    pub(super) fn libc_g(v: f64, prec: usize) -> String {
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is 512 bytes and `snprintf` is given that length,
        // so it always NUL-terminates within bounds. `%.*g` of an f64 with
        // precision <= 17 never needs more than ~330 bytes.
        let n = unsafe {
            libc::snprintf(
                buf.as_mut_ptr().cast(),
                buf.len(),
                c"%.*g".as_ptr(),
                prec as libc::c_int,
                v,
            )
        };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf overflow");
        String::from_utf8(buf[..n as usize].to_vec()).expect("glibc writes ASCII")
    }

    /// xorshift64*, so the sample is identical on every run and every host
    /// without pulling in a PRNG crate.
    pub(super) struct XorShift64(u64);
    impl XorShift64 {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed)
        }
        pub(super) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Raw bit patterns (so NaN, infinities and subnormals arrive on their
    /// own), a decade sweep across the whole exponent range, and the
    /// boundary values `%g`'s style decision turns on.
    pub(super) fn sample() -> Vec<f64> {
        let mut out = vec![
            0.0,
            -0.0,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
            f64::MIN,
            1.0 / 3.0,
            0.1 + 0.2,
            // the rounded-exponent boundary: 9.999995 must print as C does
            9.999_995,
            99_999.5,
            999_999.5,
            0.000_099_999_95,
        ];
        for e in -320i32..=308 {
            for m in [1.0_f64, 1.5, 3.3333333, 9.999999, 9.9999995] {
                let v = m * 10f64.powi(e);
                if v.is_finite() {
                    out.push(v);
                    out.push(-v);
                }
            }
        }
        let mut rng = XorShift64::new(0x5EED_1234_ABCD_0001);
        for _ in 0..50_000 {
            out.push(f64::from_bits(rng.next()));
        }
        out
    }

    /// The printf RECORD's float conversions, pinned the same way.
    ///
    /// `printfRecord.c:56` hands the user's whole conversion straight to
    /// `epicsSnprintf`, and on POSIX `epicsVsnprintf` IS `vsnprintf`
    /// (`osdStdio.c:29`) — so for `%e %E %f %F %g %G` the record's output
    /// is glibc's, exactly, including the two-digit signed exponent, the
    /// `#` alternate form and the spelling of the non-finites.
    #[test]
    fn format_float_conv_is_byte_identical_to_glibc() {
        let values = sample();
        let mut checked = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut exempted = 0usize;
        for &conv in b"eEfFgG" {
            for prec in [0usize, 1, 3, 6, 17] {
                for alt in [false, true] {
                    for &v in &values {
                        let ours = super::format_float_conv(conv, v, prec, alt);
                        let theirs = libc_conv(conv, prec, alt, v);
                        checked += 1;
                        if ours != theirs && is_glibc_alt_g_carry_quirk(conv, alt, &ours, &theirs) {
                            exempted += 1;
                            continue;
                        }
                        if ours != theirs {
                            // One line per distinct (spec, value class):
                            // 50,000 random doubles would otherwise bury
                            // every class but the first under one.
                            let class = if v.is_nan() {
                                "nan"
                            } else if v.is_infinite() {
                                "inf"
                            } else if v == 0.0 {
                                "zero"
                            } else if v.abs() < f64::MIN_POSITIVE {
                                "subnormal"
                            } else {
                                "normal"
                            };
                            let key = format!(
                                "%{}.{prec}{} {class}{}",
                                if alt { "#" } else { "" },
                                conv as char,
                                if v.is_sign_negative() { "-" } else { "+" }
                            );
                            if seen.insert(key.clone()) {
                                mismatches.push(format!(
                                    "{key}: {v:?} -> ours {ours:?} != glibc {theirs:?}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {checked} samples disagree with glibc:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
        // Pinned so the exemption cannot quietly widen to cover a real
        // divergence introduced later.
        assert_eq!(
            exempted, 18,
            "the glibc %#g carry quirk should cover exactly these samples"
        );
    }

    /// The ONE case where this crate deliberately does not reproduce
    /// glibc: `%#g` whose rounding carries into a new decade and lands in
    /// style `e`. Measured on glibc 2.39, `%#.3g` of `999.9999` is
    /// `1.e+03` and of `1000.0` is `1.00e+03` — the `#` flag's "trailing
    /// zeros are not removed" (C99 7.19.6.1p6) is lost exactly when the
    /// carry happens. `%#.3e` of the same value is `1.000e+03`, so it is
    /// specific to `%g`.
    ///
    /// Reproducing it would be wrong beyond this host: the record's text
    /// is whatever `epicsSnprintf` -> `vsnprintf` gives (`osdStdio.c:29`),
    /// which on the RTEMS and VxWorks targets is newlib, not glibc. So
    /// `format_g_val` emits the C99 form and the differential skips these
    /// samples rather than pinning one libc's bug into the port.
    fn is_glibc_alt_g_carry_quirk(conv: u8, alt: bool, ours: &str, theirs: &str) -> bool {
        if !alt || !matches!(conv, b'g' | b'G') {
            return false;
        }
        let sep = if conv == b'G' { 'E' } else { 'e' };
        let (Some((ours_mant, ours_exp)), Some((theirs_mant, theirs_exp))) =
            (ours.split_once(sep), theirs.split_once(sep))
        else {
            return false;
        };
        // Same exponent, and glibc's mantissa is ours with the `#`-kept
        // trailing zeros dropped. Matched on the text so a divergence of
        // any other shape is still a failure.
        ours_exp == theirs_exp
            && ours_mant.ends_with('0')
            && ours_mant.trim_end_matches('0') == theirs_mant
    }

    /// glibc `printf("%[#].<prec><conv>", v)` for one float conversion.
    fn libc_conv(conv: u8, prec: usize, alt: bool, v: f64) -> String {
        let spec = format!("%{}.{prec}{}\0", if alt { "#" } else { "" }, conv as char);
        let mut buf = [0u8; 512];
        // SAFETY: `spec` is NUL-terminated and takes exactly one f64;
        // `buf` is passed with its own length so `snprintf` stays in
        // bounds and NUL-terminates.
        let n =
            unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), spec.as_ptr().cast(), v) };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf overflow");
        String::from_utf8(buf[..n as usize].to_vec()).expect("glibc writes ASCII")
    }

    #[test]
    fn format_g_is_byte_identical_to_glibc() {
        let values = sample();
        let mut checked = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        for prec in [1usize, 3, 6, 17] {
            for &v in &values {
                let ours = format_g(v, prec);
                let theirs = libc_g(v, prec);
                checked += 1;
                if ours != theirs && mismatches.len() < 20 {
                    mismatches.push(format!(
                        "%.{prec}g of {v:?} (bits {:#018x}): ours {ours:?} != glibc {theirs:?}",
                        v.to_bits()
                    ));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {checked} samples disagree with glibc:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }

    /// glibc `printf("%u", (unsigned)v)`.
    fn libc_u32(v: i32) -> String {
        let mut buf = [0u8; 64];
        // SAFETY: the spec is NUL-terminated and takes exactly one
        // `unsigned int`; `buf` is passed with its own length.
        let n = unsafe {
            libc::snprintf(
                buf.as_mut_ptr().cast(),
                buf.len(),
                c"%u".as_ptr().cast(),
                v as libc::c_uint,
            )
        };
        assert!(n >= 0 && (n as usize) < buf.len());
        String::from_utf8(buf[..n as usize].to_vec()).expect("glibc writes ASCII")
    }

    /// The literal `%u` of -1 that
    /// `integer_conversions_narrow_by_dbr_type_not_through_f64` asserts,
    /// pinned to glibc rather than to arithmetic done by hand.
    #[test]
    fn u32_of_minus_one_is_what_glibc_prints() {
        assert_eq!(libc_u32(-1i32), "4294967295");
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

    /// 08 L-3 — an integer conversion is NOT routed through `f64`, and the
    /// length modifier picks the width.
    ///
    /// C `printfRecord.c:186-227` reads the link with the DBR type the
    /// conversion and modifier select — `hh`->`DBR_CHAR`, `h`->`DBR_SHORT`,
    /// `ll`->`DBR_INT64`, else `DBR_LONG`, and the unsigned family maps to the
    /// `U` twins — then hands that C-typed value to `epicsSnprintf`
    /// (`GET_PRINT`, `:47-56`). The narrowing therefore happens BEFORE any
    /// digit is formatted.
    ///
    /// Both halves of the row were live. `%lld` of an Int64 past 2^53 printed
    /// rounded digits, because the value went through `to_f64` first; and
    /// `h`/`hh` were parsed and then discarded, so `%hd` of 40000 printed
    /// 40000 where C prints the `epicsInt16` conversion.
    #[test]
    fn integer_conversions_narrow_by_dbr_type_not_through_f64() {
        // Exact past 2^53 — the f64 waypoint is gone. 2^53 + 1 is the
        // smallest integer an f64 cannot represent.
        let big = (1i64 << 53) + 1;
        let mut rec = rec_with("%lld");
        rec.set_input(0, EpicsValue::Int64(big));
        rec.process().unwrap();
        assert_eq!(rec.val, format!("{big}"), "%lld must not round through f64");

        // Default width is DBR_LONG: 32-bit, so the same value truncates.
        let mut rec = rec_with("%d");
        rec.set_input(0, EpicsValue::Int64(big));
        rec.process().unwrap();
        assert_eq!(rec.val, format!("{}", big as i32));

        // `h` = DBR_SHORT: 40000 does not fit an epicsInt16.
        let mut rec = rec_with("%hd");
        rec.set_input(0, EpicsValue::Long(40000));
        rec.process().unwrap();
        assert_eq!(rec.val, "-25536");

        // `hh` = DBR_CHAR: 300 does not fit an epicsInt8.
        let mut rec = rec_with("%hhd");
        rec.set_input(0, EpicsValue::Long(300));
        rec.process().unwrap();
        assert_eq!(rec.val, "44");

        // The unsigned family is 32-bit by default (DBR_ULONG), so -1 is
        // 4294967295 — not the 64-bit reinterpretation the old
        // `as i64 as u64` produced.
        let mut rec = rec_with("%u");
        rec.set_input(0, EpicsValue::Long(-1));
        rec.process().unwrap();
        assert_eq!(rec.val, "4294967295");

        let mut rec = rec_with("%hx");
        rec.set_input(0, EpicsValue::Long(-1));
        rec.process().unwrap();
        assert_eq!(rec.val, "ffff");

        let mut rec = rec_with("%llx");
        rec.set_input(0, EpicsValue::Int64(-1));
        rec.process().unwrap();
        assert_eq!(rec.val, "ffffffffffffffff");

        let mut rec = rec_with("%hhx");
        rec.set_input(0, EpicsValue::Long(-1));
        rec.process().unwrap();
        assert_eq!(rec.val, "ff");

        // `%c` always reads DBR_CHAR, whatever the modifier says
        // (`printfRecord.c:189`).
        let mut rec = rec_with("%d/%o");
        rec.set_input(0, EpicsValue::Long(-1));
        rec.set_input(1, EpicsValue::Long(8));
        rec.process().unwrap();
        assert_eq!(rec.val, "-1/10");
    }

    /// `h` on a float conversion reads `DBR_FLOAT`, so the value is rounded
    /// to single precision before formatting (`printfRecord.c:220-222`).
    #[test]
    fn h_on_a_float_conversion_rounds_through_f32() {
        let v = 0.12345678901234568f64;
        let mut rec = rec_with("%.10f");
        rec.set_input(0, EpicsValue::Double(v));
        rec.process().unwrap();
        let full = rec.val.clone();

        let mut rec = rec_with("%.10hf");
        rec.set_input(0, EpicsValue::Double(v));
        rec.process().unwrap();
        assert_eq!(rec.val, format!("{:.10}", v as f32 as f64));
        assert_ne!(rec.val, full, "h must actually narrow to single precision");
    }

    /// C's modifier state machine rejects a mixed or over-long run
    /// (`printfRecord.c:142-155` set `F_BADFMT`), and `F_BADFMT` prints the
    /// format text itself rather than a converted value (`:306`).
    #[test]
    fn mixed_length_modifiers_are_a_bad_format() {
        for fmt in ["%hld", "%lhd", "%hhhd", "%llld"] {
            let mut rec = rec_with(fmt);
            rec.set_input(0, EpicsValue::Long(7));
            rec.process().unwrap();
            assert_eq!(rec.val, fmt, "{fmt} must be F_BADFMT, got {:?}", rec.val);
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

    /// C `%g` chooses fixed vs scientific from the exponent of the
    /// value AFTER rounding to the requested significant digits (C99
    /// 7.19.6.1p8). At precision 6, 9.9999995e-05 rounds to 0.0001 and
    /// the exponent carries -5 -> -4, which is no longer < -4, so the
    /// style flips to fixed. Taking `log10` of the raw value emitted the
    /// scientific form instead.
    #[test]
    fn g_style_comes_from_the_rounded_exponent() {
        let mut rec = rec_with("%g");
        rec.set_input(0, EpicsValue::Double(9.9999995e-05));
        rec.process().unwrap();
        assert_eq!(rec.val, "0.0001");
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
