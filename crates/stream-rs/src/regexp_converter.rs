//! `RegexpConverter.cc`: Perl regular expressions (PCRE) `%/regexp/` and
//! `%#/regexp/subst/`, against PCRE 8.45 from [`stream_pcre_sys`].
//!
//! As in C, the pattern compiled in `parse` is never freed: it is not a
//! run-time leak, and `info` holds its pointer for the life of the
//! protocol. A maximum of 9 subexpressions is supported. Only one of them
//! can be the result of the match.

use std::borrow::Cow;

use crate::buffer::StreamBuffer;
use crate::cmsg;
use crate::converter::{ESC, StreamFormatConverter};
use crate::cstr::{CPtr, c_strchr};
use crate::debug;
use crate::error::{Ch, stream_error};
use crate::format::*;

/// The PCRE1 calls RegexpConverter.cc makes.
mod pcre {
    use std::ffi::{CStr, c_char, c_int};
    use std::ptr;

    use stream_pcre_sys as sys;

    /// A compiled pattern; never freed.
    pub(super) type Code = *mut sys::pcre;

    /// `pcre_compile(pattern, 0, &errormsg, &eoffset, NULL)`, with
    /// `pattern` ending in 0x00.
    pub(super) fn compile(pattern: &[u8]) -> Result<Code, (&'static [u8], i32)> {
        assert_eq!(pattern.last(), Some(&0));
        let mut errormsg: *const c_char = ptr::null();
        let mut eoffset: c_int = 0;
        // SAFETY: `pattern` is NUL-terminated and outlives the call.
        let code = unsafe {
            sys::pcre_compile(
                pattern.as_ptr().cast(),
                0,
                &mut errormsg,
                &mut eoffset,
                ptr::null(),
            )
        };
        if code.is_null() {
            // SAFETY: on failure PCRE points errormsg at one of its static
            // message strings.
            Err((unsafe { CStr::from_ptr(errormsg) }.to_bytes(), eoffset))
        } else {
            Ok(code)
        }
    }

    /// `pcre_fullinfo(code, NULL, PCRE_INFO_CAPTURECOUNT, &n)`
    pub(super) fn capture_count(code: Code) -> i32 {
        let mut n: c_int = 0;
        // SAFETY: `code` came from `compile` and is never freed.
        unsafe {
            sys::pcre_fullinfo(
                code,
                ptr::null(),
                sys::PCRE_INFO_CAPTURECOUNT,
                (&raw mut n).cast(),
            );
        }
        n
    }

    /// `pcre_exec(code, NULL, subject, length, 0, 0, ovector, 30)`
    pub(super) fn exec(code: Code, subject: &[u8], ovector: &mut [i32; 30]) -> i32 {
        let length = c_int::try_from(subject.len()).expect("subject cut to INT_MAX");
        // SAFETY: `code` came from `compile` and is never freed; PCRE reads
        // `length` bytes of `subject` and writes at most 30 ints.
        unsafe {
            sys::pcre_exec(
                code,
                ptr::null(),
                subject.as_ptr().cast(),
                length,
                0,
                0,
                ovector.as_mut_ptr(),
                30,
            )
        }
    }
}

/// `extract<pcre*>(info)`: the pattern `parse` put at the start of `info`.
fn code(info: &[u8]) -> pcre::Code {
    let mut bytes = [0; size_of::<usize>()];
    bytes.copy_from_slice(&info[..size_of::<usize>()]);
    usize::from_ne_bytes(bytes) as pcre::Code
}

/// The `length` bytes at `input`. C reads them in place, on into the input
/// buffer's zero tail; past the data they read 0x00 here too.
fn subject(input: CPtr<'_>, length: usize) -> Cow<'_, [u8]> {
    let data = input.base().get(input.offset()..).unwrap_or(&[]);
    if data.len() >= length {
        Cow::Borrowed(&data[..length])
    } else {
        let mut padded = data.to_vec();
        padded.resize(length, 0);
        Cow::Owned(padded)
    }
}

struct RegexpConverter;

impl StreamFormatConverter for RegexpConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if !scan_format && fmt.flags & ALT_FLAG == 0 {
            stream_error(b"Format conversion %/regexp/ is only allowed in input formats\n");
            return 0;
        }
        // C checks prec as a sub-expression index here and after compiling
        // (RegexpConverter.cc:62, :103) for %#/ too, where it counts
        // matches, so it refuses formats.html's own %#+-10.2/ab/X/.
        let subexpr_prec = fmt.flags & ALT_FLAG == 0;
        if subexpr_prec && fmt.prec > 9 {
            stream_error(format!("Sub-expression index {} too big (>9)\n", fmt.prec).as_bytes());
            return 0;
        }

        let mut pattern = StreamBuffer::new();
        while source.peek() != b'/' {
            if source.peek() == 0 {
                stream_error(&cmsg!(
                    b"Missing closing '/' after %/",
                    pattern.cstr(),
                    b" format conversion\n"
                ));
                return 0;
            }
            if source.peek() == ESC {
                // handle escaped chars
                source.advance(1);
                if source.peek() != b'/' {
                    // just un-escape /
                    pattern.append_byte(b'\\');
                    if source.peek() & 0x7f < 0x30 {
                        // handle control chars; C's char is signed, so
                        // 0x80 on prints as ffffff80
                        let c = source.peek() as i8 as i32 as u32;
                        pattern.print(format_args!("x{c:02x}"));
                        source.advance(1);
                        continue;
                    }
                    // fall through for PCRE codes like \B
                }
            }
            pattern.append_byte(source.peek());
            source.advance(1);
        }
        source.advance(1);
        debug!(b"regexp = \"", pattern.expand(), b"\"\n");

        let code = match pcre::compile(&[pattern.bytes(), &[0]].concat()) {
            Ok(code) => code,
            Err((errormsg, eoffset)) => {
                stream_error(&cmsg!(
                    errormsg,
                    b" after \"",
                    pattern.expand_range(0, eoffset as isize),
                    b"\"\n"
                ));
                return 0;
            }
        };
        let nsubexpr = pcre::capture_count(code);
        if subexpr_prec && fmt.prec > nsubexpr as i64 {
            stream_error(
                format!(
                    "Sub-expression index is {} but pattern has only {} sub-expression\n",
                    fmt.prec, nsubexpr
                )
                .as_bytes(),
            );
            return 0;
        }
        info.append(&(code as usize).to_ne_bytes());

        if fmt.flags & ALT_FLAG != 0 {
            let mut subst = StreamBuffer::new();

            debug!(
                b"check for subst in \"",
                StreamBuffer::from_bytes(source.cstr()).expand(),
                b"\"\n"
            );
            while source.peek() != b'/' {
                if source.peek() == 0 {
                    stream_error(&cmsg!(
                        b"Missing closing '/' after %#/",
                        pattern.cstr(),
                        b"/",
                        subst.cstr(),
                        b" format conversion\n"
                    ));
                    return 0;
                }
                if source.peek() == ESC {
                    subst.append_byte(ESC);
                    source.advance(1);
                }
                subst.append_byte(source.peek());
                source.advance(1);
            }
            source.advance(1);
            debug!(b"subst = \"", subst.expand(), b"\"\n");
            info.append(subst.bytes()).append_byte(0);
            return PSEUDO_FORMAT;
        }
        STRING_FORMAT
    }

    fn scan_string(
        &self,
        fmt: &StreamFormat,
        input: CPtr<'_>,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        let mut ovector = [0; 30];
        let code = code(fmt.info);
        let mut length = if fmt.width > 0 {
            fmt.width as usize
        } else {
            input.strlen()
        };
        let subexpr = if fmt.prec > 0 { fmt.prec as usize } else { 0 };

        if length > i32::MAX as usize {
            length = i32::MAX as usize;
        }
        debug!(b"input = \"", input.cstr(), b"\"\n");
        debug!(format!("length={length}\n"));

        let subject = subject(input, length);
        let rc = pcre::exec(code, &subject, &mut ovector);
        debug!(
            b"pcre_exec match \"",
            CPtr::new(&subject).cstr(),
            format!("\" result = {rc}\n")
        );
        if (subexpr != 0 && rc <= subexpr as i32) || rc < 0 {
            // error or no match or not enough sub-expressions
            return -1;
        }
        if fmt.flags & SKIP_FLAG != 0 {
            return ovector[subexpr * 2 + 1] as isize;
        }

        // An unset group is -1 to -1: empty.
        let start = ovector[subexpr * 2].max(0) as usize;
        let mut l = (ovector[subexpr * 2 + 1] - ovector[subexpr * 2]) as usize;
        if l >= *size {
            if fmt.flags & SIGN_FLAG == 0 {
                stream_error(&cmsg!(
                    b"Regexp: Matching string \"",
                    StreamBuffer::from_bytes(&subject[start..start + l]).expand(),
                    format!(
                        "\" too long ({l}>{} bytes). You may want to try the + flag: \"%+/.../\"\n",
                        size.wrapping_sub(1)
                    )
                ));
                return -1;
            }
            if *size == 0 {
                // C's size-1 wraps here and copies past the buffer; there
                // is no room even for the terminator.
                return ovector[1] as isize;
            }
            l = *size - 1;
        }
        value[..l].copy_from_slice(&subject[start..start + l]);
        value[l] = 0;
        *size = l + 1; // update number of bytes written to value
        ovector[1] as isize // consume input until end of match
    }

    fn scan_pseudo(
        &self,
        fmt: &StreamFormat,
        input: &mut StreamBuffer,
        cursor: &mut usize,
    ) -> isize {
        // re-write input buffer
        regsubst(fmt, input, *cursor);
        0
    }

    fn print_pseudo(&self, fmt: &StreamFormat, output: &mut StreamBuffer) -> bool {
        // re-write output buffer
        regsubst(fmt, output, 0);
        true
    }
}

/// `len` bytes of `buffer` at `at + offset`, `offset` being an ovector
/// entry: an unset group is -1 to -1, empty.
fn group(buffer: &StreamBuffer, at: usize, offset: i32, len: i32) -> &[u8] {
    if len == 0 {
        return &[];
    }
    let from = at + offset as usize;
    &buffer.bytes()[from..from + len as usize]
}

fn regsubst(fmt: &StreamFormat, buffer: &mut StreamBuffer, start: usize) {
    let code = code(fmt.info);
    let subst = CPtr::new(&fmt.info[size_of::<usize>()..]).cstr();
    let mut start = start;
    let mut ovector = [0; 30];
    let mut s = StreamBuffer::new();

    // C's size_t wraps for a start past the end; StreamCore's cursor never
    // is.
    let mut length = buffer.length().saturating_sub(start);
    if fmt.width != 0 && (fmt.width as usize) < length {
        length = fmt.width as usize;
    }
    if length > i32::MAX as usize {
        length = i32::MAX as usize;
    }
    if fmt.flags & LEFT_FLAG != 0 {
        start = buffer.length() - length;
    }

    debug!(
        b"regsubst buffer=\"",
        buffer.expand(),
        format!("\", start={start}, length={length}, subst = \""),
        StreamBuffer::from_bytes(subst).expand(),
        b"\"\n"
    );

    let mut c = 0;
    let mut n: i64 = 1;
    while c < length {
        let rc = pcre::exec(
            code,
            &buffer.bytes()[start + c..start + length],
            &mut ovector,
        );
        debug!(
            b"pcre_exec match \"",
            buffer.expand_range((start + c) as isize, (length - c) as isize),
            format!("\" result = {rc}\n")
        );

        if rc < 0 {
            // no match
            debug!(b"pcre_exec: no match\n");
            break;
        }
        let at = start + c;
        let l = ovector[1] - ovector[0];

        // no prec: replace all matches
        // prec with + flag: replace first prec matches
        // prec without + flag: replace only match number prec

        if fmt.flags & SIGN_FLAG != 0 || n >= fmt.prec {
            // replace subexpressions
            debug!(
                format!("before [{}]= \"", ovector[0]),
                buffer.expand_range(at as isize, ovector[0] as isize),
                b"\"\n"
            );
            debug!(
                format!("match  [{l}]= \""),
                buffer.expand_range(at as isize + ovector[0] as isize, l as isize),
                b"\"\n"
            );
            for r in 1..rc.max(0) as usize {
                debug!(
                    format!("sub{r} = \""),
                    buffer.expand_range(
                        at as isize + ovector[r * 2] as isize,
                        (ovector[r * 2 + 1] - ovector[r * 2]) as isize
                    ),
                    b"\"\n"
                );
            }
            debug!(
                b"after     = \"",
                buffer.expand_from(at as isize + ovector[1] as isize),
                b"\"\n"
            );
            s.set(subst);
            debug!(b"subs      = \"", s.expand(), b"\"\n");
            let mut r: i32 = 0;
            while r < s.length() as i32 {
                let ri = r as isize;
                debug!(b"check \"", s.expand_from(ri), b"\"\n");
                if s.get(ri) == ESC {
                    let ch = s.get(ri + 1);
                    if c_strchr(b"ulUL", ch as i32) {
                        let mut br = s.get(ri + 2).wrapping_sub(b'0');
                        if br == b'&'.wrapping_sub(b'0') {
                            br = 0;
                        }
                        debug!(
                            b"found case conversion \\",
                            Ch(ch as i32),
                            format!("{br}\n")
                        );
                        if br as i32 >= rc {
                            s.remove(ri, 1);
                            r += 1;
                            continue;
                        }
                        let br = br as usize * 2;
                        let rl = ovector[br + 1] - ovector[br];
                        s.replace(ri, 3, group(buffer, at, ovector[br], rl));
                        // The inserted group is scanned on from its second
                        // byte, as in C.
                        match ch {
                            b'u' => upper(&mut s, ri),
                            b'l' => lower(&mut s, ri),
                            b'U' => (0..rl as isize).for_each(|i| upper(&mut s, ri + i)),
                            b'L' => (0..rl as isize).for_each(|i| lower(&mut s, ri + i)),
                            _ => {}
                        }
                    } else if ch != 0 && (ch as i32) < rc {
                        // escaped 1 - 9 : replace with subexpr
                        debug!(format!("found escaped \\{ch}\n"));
                        let ch = ch as usize * 2;
                        let rl = ovector[ch + 1] - ovector[ch];
                        debug!(
                            format!("yes, replace \\{}: \"", ch / 2),
                            buffer.expand_range(at as isize + ovector[ch] as isize, rl as isize),
                            b"\"\n"
                        );
                        s.replace(ri, 2, group(buffer, at, ovector[ch], rl));
                        r += rl - 1;
                    } else {
                        debug!(format!("use literal \\{ch}\n"));
                        s.remove(ri, 1); // just remove escape
                    }
                } else if s.get(ri) == b'&' {
                    // unescaped & : replace with match
                    debug!(
                        b"replace &: \"",
                        buffer.expand_range(at as isize + ovector[0] as isize, l as isize),
                        b"\"\n"
                    );
                    s.replace(ri, 1, group(buffer, at, ovector[0], l));
                    r += l - 1;
                } else {
                    r += 1;
                    continue;
                }
                debug!(b"subs = \"", s.expand(), b"\"\n");
                r += 1;
            }
            buffer.replace(at as isize + ovector[0] as isize, l as isize, s.bytes());
            length = length - l as usize + s.length();
            c += s.length();
        } else {
            // Skip the match too: C (RegexpConverter.cc:287) moves only to
            // its start and finds it again, so %#.2/a/X/ replaces the first
            // a where formats.html says match number prec.
            c += l as usize;
        }
        c += ovector[0] as usize;
        if l == 0 {
            debug!(b"pcre_exec: empty match\n");
            c += 1; // Empty strings may lead to an endless loop. Match them only once.
        }
        if n == fmt.prec {
            // max match reached
            debug!(format!("pcre_exec: max match {n} reached\n"));
            break;
        }
        n += 1;
    }
    debug!(b"pcre_exec converted string: ", buffer.expand(), b"\n");
}

/// `if (islower(s[i])) s[i] = toupper(s[i])`, in the C locale.
fn upper(s: &mut StreamBuffer, i: isize) {
    let c = s.get(i);
    if c.is_ascii_lowercase() {
        s.set_byte(i, c.to_ascii_uppercase());
    }
}

/// `if (isupper(s[i])) s[i] = tolower(s[i])`, in the C locale.
fn lower(s: &mut StreamBuffer, i: isize) {
    let c = s.get(i);
    if c.is_ascii_uppercase() {
        s.set_byte(i, c.to_ascii_lowercase());
    }
}

/// `RegisterConverter (RegexpConverter, "/")`
pub(crate) fn registered() -> (&'static str, &'static [u8], Box<dyn StreamFormatConverter>) {
    ("RegexpConverter", b"/", Box::new(RegexpConverter))
}
