//! The one C escape table — libCom `epicsString.c`.
//!
//! EPICS escapes a byte in exactly one way, and the callers that print a
//! terminator, a trace line, a record's TINP or an echo mismatch all reach the
//! same table:
//!
//! ```text
//! \a \b \f \n \r \t \v \\ \' \"   the named escapes
//! the byte itself                 when isprint() in the C locale
//! \xNN                            everything else, lower-case hex
//! ```
//!
//! There are two C entry points and they differ on **one byte**:
//!
//! - [`escaped_from_raw`] — `epicsStrnEscapedFromRaw` (epicsString.c:120-160),
//!   which prints NUL as `\0` (:145) and writes into a *sized* destination.
//!   `epicsStrSnPrintEscaped` is a macro alias for it (epicsString.h:125). This
//!   is what `asynShowEos` (asynShellCommands.c:305), the echo interpose
//!   (asynInterposeEcho.c:73-74), the trace errlog branch (asynManager.c:3159)
//!   and every `asynRecord` field (asynRecord.c:725,1629,2005) escape through —
//!   each with its own buffer size, which the caller must state.
//! - [`print_escaped`] — `epicsStrPrintEscaped` (epicsString.c:230-262), the
//!   `FILE *` form, which has no NUL case and so falls to `\x00` (:259). This is
//!   what `asynPortDriver::report` prints the EOS pair with
//!   (asynPortDriver.cpp:3687,3690).
//!
//! Every escaping caller in this crate names one of the two, so the table exists
//! once. A private per-caller table is how the report came to escape `\r` and
//! `\n` and write a `\x03` terminator raw into stdout (R16-48).

/// C `epicsStrnEscapedFromRaw(dst, dstlen, src, srclen)` (epicsString.c:120-160)
/// — NUL escapes as `\0`, and **the destination bound is part of the call**.
///
/// C's `OUT` macro is `ndst++; if (--rem > 0) *dst++ = chr` with `rem = dstlen`:
/// at most `dstlen - 1` escaped characters reach the buffer before the closing
/// NUL, and the cut falls wherever the count runs out — *inside* an escape pair
/// if that is where it lands, leaving a dangling backslash. Compiled libCom
/// (`epicsStrnEscapedFromRaw`, 200 raw CRLF bytes, `dstlen = 40`) returns 400
/// and leaves a 39-char string ending `\r\n\r\`. Every caller therefore states
/// the size of the C buffer it is writing into; there is no unbounded form of
/// this function in C.
pub(crate) fn escaped_from_raw(src: &[u8], dstlen: usize) -> String {
    escape(src, "\\0", dstlen.saturating_sub(1))
}

/// C `epicsStrPrintEscaped` (epicsString.c:230-262) — the `FILE *` form, whose
/// `switch` has no NUL case, so a NUL takes the `\xNN` default: `\x00`. It
/// writes to a stream, so it has no destination bound.
///
/// It takes an explicit `len`, yet guards on `strlen(s) == 0` (`:236-237`):
/// a buffer whose **first byte** is NUL is "no work to do" and prints
/// *nothing*, however many bytes follow it. A NUL anywhere later is escaped
/// normally — `strlen` only looks at the first one. Compiled libCom:
/// `epicsStrPrintEscaped(fp, "\0a", 2)` returns 0 and prints nothing, while
/// `("a\0b", 3)` prints `a\x00b`. This is a C quirk, not a Rust convenience:
/// the ESCAPE trace form on a `FILE *` sink prints an empty data line for a
/// payload that starts with NUL.
pub(crate) fn print_escaped(src: &[u8]) -> String {
    if src.first().is_none_or(|&c| c == 0) {
        return String::new();
    }
    escape(src, "\\x00", usize::MAX)
}

/// The shared table. `max` is the number of escaped characters that fit in the
/// destination — C's `dstlen - 1`, or `usize::MAX` for the stream form. The cut
/// is per *character*, not per escape pair, which is what produces C's dangling
/// backslash.
fn escape(src: &[u8], nul: &str, max: usize) -> String {
    let mut out = String::with_capacity(src.len().min(max));
    let mut hex = String::new();
    for &c in src {
        let esc = match c {
            0x07 => "\\a",
            0x08 => "\\b",
            0x0c => "\\f",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x0b => "\\v",
            b'\\' => "\\\\",
            b'\'' => "\\'",
            b'"' => "\\\"",
            0 => nul,
            // C `isprint` in the C locale: the printable ASCII range.
            0x20..=0x7e => {
                hex.clear();
                hex.push(c as char);
                &hex
            }
            _ => {
                use std::fmt::Write;
                hex.clear();
                let _ = write!(hex, "\\x{c:02x}");
                &hex
            }
        };
        for ch in esc.chars() {
            if out.len() >= max {
                return out;
            }
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two C entry points agree on every byte but NUL — one case per escape
    /// class, plus the byte they disagree on.
    #[test]
    fn the_table_is_c_s_and_the_two_forms_differ_only_on_nul() {
        let named = b"\x07\x08\x0c\n\r\t\x0b\\'\"";
        assert_eq!(escaped_from_raw(named, 80), r#"\a\b\f\n\r\t\v\\\'\""#);
        assert_eq!(print_escaped(named), r#"\a\b\f\n\r\t\v\\\'\""#);

        // isprint → the byte; anything else → lower-case \xNN.
        assert_eq!(escaped_from_raw(b" ~OK", 80), " ~OK");
        assert_eq!(
            escaped_from_raw(b"\x03\x1b\x7f\xff", 80),
            r"\x03\x1b\x7f\xff"
        );
        assert_eq!(print_escaped(b"\x03\x1b\x7f\xff"), r"\x03\x1b\x7f\xff");

        // The one divergence: epicsStrnEscapedFromRaw has a `case '\0'`
        // (epicsString.c:145); epicsStrPrintEscaped does not (:255-260).
        assert_eq!(escaped_from_raw(b"a\0b", 80), r"a\0b");
        assert_eq!(print_escaped(b"a\0b"), r"a\x00b");
    }

    /// C's destination bound, byte for byte against compiled libCom
    /// (`epicsStrnEscapedFromRaw` linked from epics-base):
    ///
    /// ```text
    /// 200 CRLF -> 40   ret=400  strlen=39  [\r\n...\r\]
    /// 200 CRLF -> 10   ret=400  strlen= 9  [\r\n\r\n\]
    /// "a\tb"   ->  4   ret=  4  strlen= 3  [a\t]
    /// ```
    ///
    /// The cut is at `dstlen - 1` *characters* of the escaped stream, so it can
    /// fall between the backslash and the letter of an escape pair and leave the
    /// backslash dangling. That is C's output and it is what the record fields
    /// and the trace buffer actually hold.
    #[test]
    fn the_destination_bound_truncates_the_escaped_stream_mid_pair() {
        let crlf: Vec<u8> = b"\r\n".repeat(100);

        let tinp = escaped_from_raw(&crlf, 40);
        assert_eq!(tinp.len(), 39);
        assert!(tinp.ends_with(r"\r\n\r\"), "dangling backslash: {tinp}");

        let eos = escaped_from_raw(&crlf, 10);
        assert_eq!(eos, r"\r\n\r\n\");

        // The pair that does not fit is cut, not dropped whole.
        assert_eq!(escaped_from_raw(b"a\tb", 4), r"a\t");
        assert_eq!(escaped_from_raw(b"a\tb", 3), r"a\");

        // Exactly-fits and under-fits leave the escaped text intact.
        assert_eq!(escaped_from_raw(b"\r\n", 5), r"\r\n");
        assert_eq!(escaped_from_raw(b"", 40), "");

        // A four-character \xNN pair is cut the same way.
        assert_eq!(escaped_from_raw(b"\xff", 3), r"\x");
    }

    /// R17-49. `epicsStrPrintEscaped` early-returns on `strlen(s) == 0`
    /// (epicsString.c:236-237) even though it was handed an explicit length: a
    /// buffer whose first byte is NUL prints nothing at all. Against compiled
    /// libCom:
    ///
    /// ```text
    /// epicsStrPrintEscaped(fp, "\0a", 2)  -> ret 0, prints nothing
    /// epicsStrPrintEscaped(fp, "a\0b", 3) -> ret 6, prints a\x00b
    /// epicsStrPrintEscaped(fp, "",   0)   -> ret 0, prints nothing
    /// ```
    ///
    /// `epicsStrnEscapedFromRaw` has no such guard — it loops on `srclen` and
    /// escapes the leading NUL as `\0` (compiled: `"\0a"` → `\0a`).
    #[test]
    fn print_escaped_prints_nothing_when_the_first_byte_is_nul() {
        assert_eq!(print_escaped(b"\0a"), "");
        assert_eq!(print_escaped(b"\0"), "");
        assert_eq!(print_escaped(b""), "");

        // Only the FIRST byte: strlen stops there, and a later NUL still escapes.
        assert_eq!(print_escaped(b"a\0b"), r"a\x00b");

        // The bounded form keeps escaping — the guard is C's, and C put it in
        // one function only.
        assert_eq!(escaped_from_raw(b"\0a", 40), r"\0a");
    }
}
