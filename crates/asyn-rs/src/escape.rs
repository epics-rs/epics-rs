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
//!   which prints NUL as `\0` (:145). `epicsStrSnPrintEscaped` is a macro alias
//!   for it (epicsString.h:125). This is what `asynShowEos`
//!   (asynShellCommands.c:305) and every `asynRecord` field
//!   (asynRecord.c:725,1629,2005) escape through.
//! - [`print_escaped`] — `epicsStrPrintEscaped` (epicsString.c:230-262), the
//!   `FILE *` form, which has no NUL case and so falls to `\x00` (:259). This is
//!   what `asynPortDriver::report` prints the EOS pair with
//!   (asynPortDriver.cpp:3687,3690).
//!
//! Every escaping caller in this crate names one of the two, so the table exists
//! once. A private per-caller table is how the report came to escape `\r` and
//! `\n` and write a `\x03` terminator raw into stdout (R16-48).

/// C `epicsStrnEscapedFromRaw` (epicsString.c:120-160) — NUL escapes as `\0`.
pub(crate) fn escaped_from_raw(src: &[u8]) -> String {
    escape(src, "\\0")
}

/// C `epicsStrPrintEscaped` (epicsString.c:230-262) — the `FILE *` form, whose
/// `switch` has no NUL case, so a NUL takes the `\xNN` default: `\x00`.
pub(crate) fn print_escaped(src: &[u8]) -> String {
    escape(src, "\\x00")
}

fn escape(src: &[u8], nul: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for &c in src {
        match c {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0 => out.push_str(nul),
            // C `isprint` in the C locale: the printable ASCII range.
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
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
        assert_eq!(escaped_from_raw(named), r#"\a\b\f\n\r\t\v\\\'\""#);
        assert_eq!(print_escaped(named), r#"\a\b\f\n\r\t\v\\\'\""#);

        // isprint → the byte; anything else → lower-case \xNN.
        assert_eq!(escaped_from_raw(b" ~OK"), " ~OK");
        assert_eq!(escaped_from_raw(b"\x03\x1b\x7f\xff"), r"\x03\x1b\x7f\xff");
        assert_eq!(print_escaped(b"\x03\x1b\x7f\xff"), r"\x03\x1b\x7f\xff");

        // The one divergence: epicsStrnEscapedFromRaw has a `case '\0'`
        // (epicsString.c:145); epicsStrPrintEscaped does not (:255-260).
        assert_eq!(escaped_from_raw(b"a\0b"), r"a\0b");
        assert_eq!(print_escaped(b"a\0b"), r"a\x00b");
    }
}
