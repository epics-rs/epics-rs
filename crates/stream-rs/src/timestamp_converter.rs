//! `TimestampConverter.cc`: `%T(<strftime format>)` prints and scans a
//! double as seconds since 1970, in local time.

use crate::buffer::StreamBuffer;
use crate::cmsg;
use crate::converter::{ESC, StreamFormatConverter};
use crate::cstr::{CPtr, c_isalpha, c_isspace, c_strtol, c_strtoul, c_tolower};
use crate::error::stream_error;
use crate::format::*;
use epics_libcom_rs::cfmt::CFloat;

pub(crate) struct TimestampConverter;

impl StreamFormatConverter for TimestampConverter {
    fn parse(
        &self,
        _fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        _scan_format: bool,
    ) -> i32 {
        if source.peek() == b'(' {
            loop {
                source.advance(1);
                match source.peek() {
                    b')' => break,
                    0 => {
                        stream_error(b"missing ')' after %T format\n");
                        return 0;
                    }
                    ESC => {
                        source.advance(1);
                        info.append_byte(source.peek());
                        if source.peek() == b'%' {
                            info.append_byte(b'%');
                        }
                    }
                    b'%' => {
                        source.advance(1);
                        // look for formatted fractions like %3f
                        if source.peek().is_ascii_digit() {
                            let (n, c) = c_strtoul(*source, 10);
                            if CPtr::at_offset(source.base(), c).peek() == b'f' {
                                source.set_offset(c);
                                info.print(format_args!("%0{}f", n as u32));
                                continue;
                            }
                        }
                        // look for nanoseconds %N of %f
                        if matches!(source.peek(), b'N' | b'f') {
                            info.append(b"%09f");
                            continue;
                        }
                        // look for seconds with fractions like %.3S
                        if source.peek() == b'.' {
                            let mut c = source.plus(1);
                            let n = if c.peek().is_ascii_digit() {
                                let (n, end) = c_strtoul(c, 10);
                                c.set_offset(end);
                                n as u32
                            } else {
                                9
                            };
                            if c.peek().eq_ignore_ascii_case(&b'S') {
                                *source = c;
                                info.print(format_args!("%{}.%0{}f", c.peek() as char, n));
                                continue;
                            }
                        }
                        // else normal format
                        info.append_byte(b'%');
                        info.append_byte(source.peek());
                    }
                    c => {
                        info.append_byte(c);
                    }
                }
            }
            source.advance(1);
            info.append_byte(0);
        } else {
            info.append(b"%Y-%m-%d %H:%M:%S").append_byte(0);
        }
        DOUBLE_FORMAT
    }

    fn print_double(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: f64) -> bool {
        // C bug not copied: C truncates toward zero, so -1.25 printed as
        // second -1 with fraction .25.
        let mut sec = value.floor() as libc::time_t;
        let frac = value - sec as f64;
        let info = escape_fractions(CPtr::new(fmt.info).cstr());
        crate::debug!(
            b"TimestampConverter::printDouble ",
            format!("{:.6}", CFloat(value)),
            b", '",
            CPtr::new(fmt.info).cstr(),
            b"'\n"
        );
        let mut buffer = ctime::strftime40(&info, &ctime::localtime(sec));
        // C bug not copied: C never carries, so .9996 as %S.%3f printed the
        // old second with "000". When the finest field rounds up to 1, the
        // second carries and every field prints zeros; otherwise each field
        // is rounded on its own, as C does.
        if let Some(n) = finest_fraction(&buffer)
            && format!("{:.n$}", CFloat(frac)).starts_with('1')
        {
            sec += 1;
            buffer = ctime::strftime40(&info, &ctime::localtime(sec));
        }
        let mut i = output.length() as isize;
        output.append(&buffer);

        // look for fractional seconds
        loop {
            i = output.find(b"%0", i);
            if i == -1 {
                break;
            }
            let (n, c) = c_strtol(output.ptr(i + 1), 10);
            if output.get(c as isize) != b'f' {
                return false;
            }
            let c = c + 1;
            // print fractional part
            let n = precision(n);
            let fraction = format!("{:.n$}", CFloat(frac));
            // C bug not copied: C crashes on %0f, where printf gives no '.'.
            let digits = fraction.get(2..).unwrap_or("");
            output.replace(i, c as isize - i, digits.as_bytes());
        }
        true
    }

    fn scan_double(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut f64) -> isize {
        scan_double_at(ctime::now(), fmt, input, value)
    }
}

/// A `%0<n>f` field's digits: printf's for C's `(int)n`, at most 12.
fn precision(n: i64) -> usize {
    let n = n as i32;
    // C sprintf's into `char fracbuffer[15]` (TimestampConverter.cc:155),
    // "0." and 12 digits; more overflows it.
    if n < 0 { 6 } else { (n as usize).min(12) }
}

/// The most digits a `%0<n>f` field in `text` asks for, among the fields
/// `printDouble` fills before one it refuses.
fn finest_fraction(text: &[u8]) -> Option<usize> {
    let mut buf = StreamBuffer::new();
    buf.append(text);
    let text = buf;
    let mut finest = None;
    let mut i = 0;
    loop {
        i = text.find(b"%0", i);
        if i == -1 {
            return finest;
        }
        let (n, c) = c_strtol(text.ptr(i + 1), 10);
        if text.get(c as isize) != b'f' {
            return finest;
        }
        finest = finest.max(Some(precision(n)));
        i = c as isize;
    }
}

/// `scanDouble`, with "today" taken from `now`, which C takes from
/// `time(NULL)` (TimestampConverter.cc scanDouble) and `scan_double` from
/// the same.
pub fn scan_double_at(
    now: libc::time_t,
    fmt: &StreamFormat,
    input: CPtr<'_>,
    value: &mut f64,
) -> isize {
    // Init time stamp with "today"
    let mut tm = ctime::localtime(now);
    tm.tm_sec = 0;
    tm.tm_min = 0;
    tm.tm_hour = 0;
    tm.tm_yday = 0;
    tm.tm_isdst = -1;
    let mut st = Scan { tm, sec: 0, ns: 0 };

    let Some(end) = scantime(input, CPtr::new(fmt.info), &mut st) else {
        stream_error(b"error parsing time\n");
        return -1;
    };
    let seconds = if st.tm.tm_mon == -1 {
        st.sec
    } else {
        let seconds = ctime::mktime(&mut st.tm);
        if seconds == -1 && st.tm.tm_yday == 0 {
            let tm = &st.tm;
            stream_error(
                format!(
                    "mktime failed for {:02}/{:02}/{:04} {:02}:{:02}:{:02}\n",
                    tm.tm_mon + 1,
                    tm.tm_mday,
                    tm.tm_year + 1900,
                    tm.tm_hour,
                    tm.tm_min,
                    tm.tm_sec
                )
                .as_bytes(),
            );
            return -1;
        }
        seconds
    };
    *value = seconds as f64 + st.ns as f64 * 1e-9;
    (end.offset() - input.offset()) as isize
}

/// What `scantime` fills in.
struct Scan {
    tm: ctime::Tm,
    /// The `%s`/`%S` seconds; `tm_sec` holds them cut to `int`, which C
    /// returns for `%s` (C bug not copied: wrong after 2038).
    sec: libc::time_t,
    ns: u64,
}

// many OS don't have strptime or strptime does not fully support
// all fields, e.g. %z.

fn strmatch(input: &mut CPtr<'_>, strings: &[&[u8]], minlen: usize) -> i32 {
    for (i, s) in strings.iter().enumerate() {
        let mut c = 0;
        loop {
            if c == s.len() {
                input.advance(c);
                return i as i32;
            }
            if c_tolower(input.at(c) as i32) != s[c] as i32 {
                if c >= minlen {
                    input.advance(c);
                    return i as i32;
                }
                break;
            }
            c += 1;
        }
    }
    -1
}

fn nummatch(input: &mut CPtr<'_>, min: i32, max: i32) -> i32 {
    let (i, c) = c_strtol(*input, 10);
    if c == input.offset() {
        return -10000;
    }
    // C stores the long in an int.
    let i = i as i32;
    if i < min || i > max {
        return -10000;
    }
    input.set_offset(c);
    i
}

/// printf `%.20s`
fn s20(p: CPtr<'_>) -> &[u8] {
    let s = p.cstr();
    &s[..s.len().min(20)]
}

// C bug not copied: C's table has no "october", so "oct" did not parse and
// "nov" and "dec" were read as October and November.
const MONTHS: [&[u8]; 12] = [
    b"january",
    b"february",
    b"march",
    b"april",
    b"may",
    b"june",
    b"july",
    b"august",
    b"september",
    b"october",
    b"november",
    b"december",
];
const AMPM: [&[u8]; 2] = [b"am", b"pm"];

fn scantime<'a>(input: CPtr<'a>, format: CPtr<'_>, st: &mut Scan) -> Option<CPtr<'a>> {
    let mut input = input;
    let mut format = format;
    let mut pm = -1;
    let mut century = -1;

    // C bug not copied: C starts zone at timezone/60, minutes WEST of UTC,
    // but reads %z as minutes EAST, so 12:00 +0100 came out as 13:00 UTC.
    let west = ctime::timezone() / 60;
    let mut zone = -west;
    crate::debug!(
        b"TimestampConverter::scantime: native time zone = ",
        zone.to_string(),
        b"\n"
    );

    macro_rules! fail {
        ($what:literal, $at:expr) => {{
            stream_error(&cmsg!(
                concat!("error parsing ", $what, ": '"),
                s20($at),
                b"'\n"
            ));
            return None;
        }};
    }

    while format.peek() != 0 {
        match format.peek() {
            b'%' => {
                crate::debug!(
                    b"TimestampConverter::scantime: input = '",
                    input.cstr(),
                    b"'\n"
                );
                format.advance(1);
                let mut conv = format.peek();
                format.advance(1);
                // Modifiers (ignore)
                while matches!(conv, b'E' | b'O') {
                    conv = format.peek();
                    format.advance(1);
                }
                match conv {
                    // constants
                    0 | b'%' => {
                        if conv == 0 {
                            // stray % at end of format string
                            format.set_offset(format.offset() - 1);
                        }
                        if input.peek() != b'%' {
                            return None;
                        }
                        input.advance(1);
                    }
                    b'n' => {
                        if input.peek() != b'\n' {
                            return None;
                        }
                        input.advance(1);
                    }
                    b't' => {
                        if input.peek() != b'\t' {
                            return None;
                        }
                        input.advance(1);
                    }
                    // ignored
                    b'A' | b'a' | b'Z' => {
                        // day of week name, time zone name
                        while c_isalpha(input.peek() as i32) {
                            input.advance(1);
                        }
                    }
                    b'u' | b'w' => {
                        // day of week number
                        let i = nummatch(&mut input, 0, 7);
                        if i < 0 {
                            fail!("day of week", input);
                        }
                        crate::debug!(
                            b"TimestampConverter::scantime: day of week = ",
                            i.to_string(),
                            b"\n"
                        );
                    }
                    b'U' | b'W' | b'V' => {
                        // week number
                        let i = nummatch(&mut input, 0, 53);
                        if i < 0 {
                            fail!("week number", input);
                        }
                        crate::debug!(
                            b"TimestampConverter::scantime: week number = ",
                            i.to_string(),
                            b"\n"
                        );
                    }
                    b'j' => {
                        // day of year
                        let i = nummatch(&mut input, 0, 366);
                        if i < 0 {
                            fail!("day of year", input);
                        }
                        crate::debug!(
                            b"TimestampConverter::scantime: day of year = ",
                            i.to_string(),
                            b"\n"
                        );
                    }
                    // date
                    b'b' | b'h' | b'B' | b'm' => {
                        // month
                        let mut i = strmatch(&mut input, &MONTHS, 3);
                        if i < 0 {
                            i = nummatch(&mut input, 1, 12);
                            if i < 0 {
                                fail!("month", input);
                            }
                            i -= 1; // Jan = 0
                        }
                        st.tm.tm_mon = i;
                        crate::debug!(
                            b"TimestampConverter::scantime: month = ",
                            (st.tm.tm_mon + 1).to_string(),
                            b" (",
                            MONTHS[st.tm.tm_mon as usize],
                            b")\n"
                        );
                    }
                    b'd' | b'e' => {
                        // day of month
                        let i = nummatch(&mut input, 1, 31);
                        if i < 0 {
                            fail!("day of month", input);
                        }
                        st.tm.tm_mday = i;
                        crate::debug!(
                            b"TimestampConverter::scantime: day = ",
                            st.tm.tm_mday.to_string(),
                            b"\n"
                        );
                    }
                    b'Y' | b'y' => {
                        // year
                        let (i, c) = c_strtol(input, 10);
                        input.set_offset(c);
                        let i = i as i32;
                        if i < 100 {
                            // 2 digit year
                            // C bug not copied: C takes 69..99 as 20xx and
                            // 00..68 as 19xx, the reverse of POSIX.
                            if century == -1 {
                                century = (i < 69) as i32;
                            }
                            st.tm.tm_year = i + century * 100; // 0 = 1900
                        } else {
                            // 4 digit year
                            st.tm.tm_year = i - 1900; // 0 = 1900
                        }
                        crate::debug!(
                            b"TimestampConverter::scantime: year = ",
                            (st.tm.tm_year + 1900).to_string(),
                            b"\n"
                        );
                    }
                    b'C' => {
                        // century
                        let i = nummatch(&mut input, 0, 99);
                        if i < 0 {
                            fail!("century", input);
                        }
                        century = i - 19;
                        // C bug not copied: C adds 100 * i to a year that
                        // counts from 1900, so "%y %C" read 23 20 as 3923.
                        st.tm.tm_year = st.tm.tm_year % 100 + 100 * century; // 0 = 1900
                        crate::debug!(
                            b"TimestampConverter::scantime: year = ",
                            (st.tm.tm_year + 1900).to_string(),
                            b"\n"
                        );
                    }
                    // time
                    b'H' | b'k' | b'I' | b'l' => {
                        // hour
                        let mut i = nummatch(&mut input, 0, 23);
                        if i < 0 {
                            fail!("hour", input);
                        }
                        if pm == 0 && i == 12 {
                            i = 0;
                        }
                        if pm == 1 && i < 12 {
                            i += 12;
                        }
                        st.tm.tm_hour = i;
                        crate::debug!(
                            b"TimestampConverter::scantime: hour = ",
                            st.tm.tm_hour.to_string(),
                            b"\n"
                        );
                    }
                    b'P' | b'p' => {
                        // AM / PM
                        let i = strmatch(&mut input, &AMPM, 1);
                        if i < 0 {
                            fail!("am/pm", input);
                        }
                        pm = i;
                        if pm == 0 && st.tm.tm_hour == 12 {
                            st.tm.tm_hour = 0;
                        }
                        if pm == 1 && st.tm.tm_hour < 12 {
                            st.tm.tm_hour += 12;
                        }
                        crate::debug!(
                            b"TimestampConverter::scantime: ",
                            if pm != 0 { "PM" } else { "AM" },
                            b" hour = ",
                            st.tm.tm_hour.to_string(),
                            b"\n"
                        );
                    }
                    b'M' => {
                        // minute
                        let i = nummatch(&mut input, 0, 59);
                        if i < 0 {
                            fail!("minute", input);
                        }
                        st.tm.tm_min = i;
                        crate::debug!(
                            b"TimestampConverter::scantime: min = ",
                            st.tm.tm_min.to_string(),
                            b"\n"
                        );
                    }
                    b'S' => {
                        // second
                        let i = nummatch(&mut input, 0, 60);
                        if i < 0 {
                            fail!("week second", input);
                        }
                        st.tm.tm_sec = i;
                        st.sec = i as libc::time_t;
                        crate::debug!(
                            b"TimestampConverter::scantime: sec = ",
                            st.tm.tm_sec.to_string(),
                            b"\n"
                        );
                    }
                    b's' => {
                        // second since 1970
                        let (i, c) = c_strtol(input, 10);
                        input.set_offset(c);
                        st.tm.tm_sec = i as i32;
                        st.sec = i as libc::time_t;
                        st.tm.tm_mon = -1;
                        st.tm.tm_isdst = 0;
                        crate::debug!(
                            b"TimestampConverter::scantime: sec = ",
                            st.tm.tm_sec.to_string(),
                            b"\n"
                        );
                    }
                    b'0' => {
                        // fractions of seconds like %09f
                        let (n, c) =
                            c_strtol(CPtr::at_offset(format.base(), format.offset() - 1), 10);
                        format.set_offset(c);
                        if format.peek() != b'f' {
                            return None;
                        }
                        format.advance(1);
                        let mut n = n as i32;
                        crate::debug!(
                            b"max ",
                            n.to_string(),
                            b" digits fraction in '",
                            input.cstr(),
                            b"'\n"
                        );
                        // C bug not copied: C scales the digits up until
                        // they reach 1e8, so ".05" read as 0.5 and ".0"
                        // never ended.
                        let mut i: u64 = 0;
                        let mut digits = 0;
                        loop {
                            let more = n != 0;
                            n = n.wrapping_sub(1);
                            if !more || !input.peek().is_ascii_digit() {
                                break;
                            }
                            if digits < 9 {
                                i = i * 10 + (input.peek() - b'0') as u64;
                                digits += 1;
                            }
                            input.advance(1);
                        }
                        i *= 10u64.pow(9 - digits);
                        st.ns = i;
                        crate::debug!(
                            b"TimestampConverter::scantime: nanosec = ",
                            i.to_string(),
                            b", rest '",
                            input.cstr(),
                            b"'\n"
                        );
                    }
                    b'z' => {
                        // time zone offset
                        let i = nummatch(&mut input, -2400, 2400);
                        if i < -2400 {
                            fail!("time zone", input);
                        }
                        zone = i / 100 * 60 + i % 100;
                        st.tm.tm_isdst = 0;
                        crate::debug!(
                            b"TimestampConverter::scantime: zone = ",
                            zone.to_string(),
                            b"\n"
                        );
                    }
                    b'+' | b'-' => {
                        // set time zone in format string
                        format.set_offset(format.offset() - 1);
                        let i = nummatch(&mut format, -2400, 2400);
                        zone = i / 100 * 60 + i % 100;
                        st.tm.tm_isdst = 0;
                        crate::debug!(
                            b"TimestampConverter::scantime: zone = ",
                            zone.to_string(),
                            b"\n"
                        );
                    }
                    // shortcuts
                    b'c' | b'D' | b'F' | b'R' | b'T' | b'x' | b'X' | b'r' => {
                        let shortcut: &[u8] = match conv {
                            b'c' => b"%a %b %d %H:%M:%S %Y\0",
                            b'D' | b'x' => b"%m/%d/%y\0",
                            b'F' => b"%Y-%m-%d\0",
                            b'R' => b"%H:%M\0",
                            b'T' => b"%H:%M:%S\0",
                            _ => b"%I:%M:%S %p\0",
                        };
                        input = scantime(input, CPtr::new(shortcut), st)?;
                    }
                    _ => {
                        format.set_offset(format.offset() - 1);
                        stream_error(&cmsg!(b"unknown time format %", format.cstr(), b"\n"));
                        return None;
                    }
                }
            }
            b' ' => {
                format.advance(1);
                while c_isspace(input.peek() as i32) {
                    input.advance(1);
                }
            }
            c => {
                if c != input.peek() {
                    stream_error(&cmsg!(
                        b"input '",
                        s20(input),
                        b"' does not match constant '",
                        s20(format),
                        b"'\n"
                    ));
                    return None;
                }
                format.advance(1);
                input.advance(1);
            }
        }
    }
    let tm = &mut st.tm;
    tm.tm_min += -west - zone;
    tm.tm_hour += tm.tm_min / 60;
    tm.tm_min %= 60;
    if tm.tm_min < 0 {
        tm.tm_min += 60;
        tm.tm_hour -= 1;
    }
    // C bug not copied: C subtracts the days an hour past 23 makes, and
    // adds 24 to tm_min for an hour below 0.
    tm.tm_mday += tm.tm_hour / 24;
    tm.tm_hour %= 24;
    if tm.tm_hour < 0 {
        tm.tm_hour += 24;
        tm.tm_mday -= 1;
    }
    Some(input)
}

/// `info` with each `%0<n>f` fraction written `%%0<n>f`, which strftime
/// gives back as `%0<n>f` for `printDouble` to find.
///
/// C bug not copied: C hands the bare `%0<n>f` to strftime, and glibc pads
/// a conversion it does not know to its width, so `%N` printed five zeros
/// before the nine digits.
fn escape_fractions(info: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(info.len() + 8);
    let mut i = 0;
    while i < info.len() {
        if info[i] == b'%' {
            let digits = info[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if info.get(i + 1) == Some(&b'0') && info.get(i + 1 + digits) == Some(&b'f') {
                out.push(b'%');
            } else if info.get(i + 1) == Some(&b'%') {
                out.extend_from_slice(b"%%");
                i += 2;
                continue;
            }
        }
        out.push(info[i]);
        i += 1;
    }
    out
}

/// The `<time.h>` calls the converter makes.
mod ctime {
    pub type Tm = libc::tm;

    fn zeroed() -> Tm {
        // SAFETY: `tm` is integers plus, on some systems, a `tm_zone`
        // pointer, for which null is valid.
        unsafe { std::mem::zeroed() }
    }

    /// `localtime_r`; all fields 0 where it fails, which C leaves
    /// uninitialised.
    pub fn localtime(t: libc::time_t) -> Tm {
        let mut tm = zeroed();
        #[cfg(unix)]
        // SAFETY: both pointers are to live locals.
        unsafe {
            libc::localtime_r(&t, &mut tm);
        }
        #[cfg(windows)]
        // SAFETY: both pointers are to live locals.
        unsafe {
            libc::localtime_s(&mut tm, &t);
        }
        tm
    }

    #[cfg(windows)]
    unsafe extern "C" {
        fn strftime(
            s: *mut libc::c_char,
            max: libc::size_t,
            format: *const libc::c_char,
            tm: *const Tm,
        ) -> libc::size_t;
    }
    #[cfg(unix)]
    use libc::strftime;

    /// `strftime` into C's `char buffer[40]`: nothing when the result does
    /// not fit.
    pub fn strftime40(format: &[u8], tm: &Tm) -> Vec<u8> {
        let mut format = format.to_vec();
        format.push(0);
        let mut buffer = [0u8; 40];
        // SAFETY: `format` is 0-terminated and `buffer` holds the 40 bytes
        // strftime is allowed to write.
        let length = unsafe {
            strftime(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                format.as_ptr().cast(),
                tm,
            )
        };
        buffer[..length].to_vec()
    }

    /// `time(NULL)`
    pub fn now() -> libc::time_t {
        // SAFETY: a null argument is allowed.
        unsafe { libc::time(std::ptr::null_mut()) }
    }

    #[cfg(windows)]
    unsafe extern "C" {
        #[link_name = "_mktime64"]
        fn c_mktime(tm: *mut Tm) -> libc::time_t;
    }
    #[cfg(unix)]
    use libc::mktime as c_mktime;

    /// `mktime`
    pub fn mktime(tm: &mut Tm) -> libc::time_t {
        // SAFETY: `tm` is a live, initialised struct.
        unsafe { c_mktime(tm) }
    }

    #[cfg(unix)]
    unsafe extern "C" {
        fn tzset();
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe extern "C" {
        #[link_name = "timezone"]
        static TIMEZONE: libc::c_long;
    }

    /// `tzset(); timezone`: seconds west of UTC, standard time.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn timezone() -> i32 {
        // SAFETY: tzset takes no argument; timezone is the C library's
        // global it sets.
        unsafe {
            tzset();
            TIMEZONE as i32
        }
    }

    /// `tzset(); timezone`, from `tm_gmtoff` as C does on the BSDs; C's
    /// BSD branch keeps tm_gmtoff's sign, east positive, which is the
    /// reverse of timezone's.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    pub fn timezone() -> i32 {
        // SAFETY: tzset takes no argument.
        unsafe { tzset() };
        let t = now();
        let half_year = 183 * 86400;
        let (a, b) = (localtime(t), localtime(t + half_year));
        let standard = if a.tm_isdst > 0 { b } else { a };
        -(standard.tm_gmtoff as i32)
    }

    /// `_tzset(); _timezone`
    #[cfg(windows)]
    pub fn timezone() -> i32 {
        let mut seconds: libc::c_long = 0;
        // SAFETY: both calls only read the environment; seconds is a live
        // local.
        unsafe {
            libc::tzset();
            libc::get_timezone(&mut seconds);
        }
        seconds as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_today(now: libc::time_t, info: &[u8], input: &[u8]) -> f64 {
        // SAFETY: this test binary's only time zone is set here, before
        // any libc time function runs in this test.
        unsafe { std::env::set_var("TZ", "CET-1CEST,M3.5.0,M10.5.0/3") };
        let info = [info, &[0]].concat();
        let input = [input, &[0]].concat();
        let fmt = StreamFormat {
            info: &info,
            ..StreamFormat::default()
        };
        let mut value = 0.0;
        assert!(scan_double_at(now, &fmt, CPtr::new(&input), &mut value) > 0);
        value
    }

    /// A time without a date is on the day `now` falls on, in that day's
    /// DST.
    #[test]
    fn a_time_alone_is_today() {
        // 2023-11-14 23:13:20 CET -> 2023-11-14 12:30 CET
        assert_eq!(scan_today(1700000000, b"%H:%M", b"12:30"), 1699961400.0);
        // 2023-07-14 14:00 CEST -> 2023-07-14 12:30 CEST
        assert_eq!(scan_today(1689336000, b"%H:%M", b"12:30"), 1689330600.0);
        // The day is kept, the rest of today is not.
        assert_eq!(scan_today(1700000000, b"%M", b"5"), 1699916700.0);
    }
}
