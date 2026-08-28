//! `stdio` device support — EPICS base `devStdio.c` (`devSoft.dbd`).
//!
//! One DTYP (`"stdio"`, INST_IO) shared by three output records — `lso`,
//! `printf`, `stringout` (C `devLsoStdio` / `devPrintfStdio` / `devSoStdio`).
//! The INST_IO `OUT` instio string names an output stream; on each write the
//! record's `VAL` string is printed to that stream followed by a newline.
//!
//! C `write_xxx` (`devStdio.c:98-104` / `:148-154` / `:198-204`):
//! `if (pstream) pstream->print("%s\n", prec->val)`. The stream is resolved
//! once by `add_xxx` (`:64-79` etc.) from `out.value.instio.string` against
//! the fixed table `outStreams[]` (`:29-37`): `stdout` → `printf`,
//! `stderr` → `vfprintf(stderr)`, `errlog` → `errlogVprintf`. An OUT naming
//! no known stream leaves `dpvt = NULL`, so the record writes nothing.

use crate::error::{CaError, CaResult};
use crate::runtime::log::errlog_printf;
use crate::server::device_support::{DeviceInitOutcome, DeviceSupport};
use crate::server::record::Record;
use crate::types::EpicsValue;

/// The output stream named by the INST_IO `OUT` instio string —
/// C `outStreams[]` (`devStdio.c:29-37`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdioStream {
    /// `"stdout"` → C `printf` (process stdout).
    Stdout,
    /// `"stderr"` → C `stderrPrintf` → `vfprintf(stderr)` (process stderr).
    Stderr,
    /// `"errlog"` → C `logPrintf` → `errlogVprintf` (the EPICS errlog).
    Errlog,
}

impl StdioStream {
    /// Resolve the instio string to a stream, or `None` for an unknown name
    /// (C `add_xxx` loops `outStreams` and leaves `dpvt = NULL` on no match).
    fn from_name(name: &str) -> Option<StdioStream> {
        match name {
            "stdout" => Some(StdioStream::Stdout),
            "stderr" => Some(StdioStream::Stderr),
            "errlog" => Some(StdioStream::Errlog),
            _ => None,
        }
    }

    /// Print `line` followed by a newline — C `pstream->print("%s\n", val)`.
    /// All three streams carry C's `\n`: `println!`/`eprintln!` supply it, and
    /// the errlog stream carries it in the message, because the errlog console
    /// writes the caller's bytes and appends nothing (C `errlog.c:795`).
    fn print_line(self, line: &str) {
        match self {
            StdioStream::Stdout => println!("{line}"),
            StdioStream::Stderr => eprintln!("{line}"),
            StdioStream::Errlog => errlog_printf(&format!("{line}\n")),
        }
    }
}

/// `stdio` device support for the `lso` / `printf` / `stringout` records.
pub struct StdioDeviceSupport {
    /// Resolved output stream; `None` when `OUT` named no known stream
    /// (C `dpvt == NULL` → `write_xxx` is a no-op).
    stream: Option<StdioStream>,
}

impl StdioDeviceSupport {
    /// Construct from the INST_IO `OUT` string
    /// ([`DeviceSupportContext::out`](crate::server::ioc_app::DeviceSupportContext)).
    /// A leading `@` (the INST_IO link marker the field carries) is stripped,
    /// matching C reading `out.value.instio.string` (the post-`@` content).
    pub fn new(out: &str) -> Self {
        let name = out.strip_prefix('@').unwrap_or(out).trim();
        Self {
            stream: StdioStream::from_name(name),
        }
    }
}

impl DeviceSupport for StdioDeviceSupport {
    fn dtyp(&self) -> &str {
        "stdio"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        // C registers a SEPARATE dset per record type (devLsoStdio /
        // devPrintfStdio / devSoStdio), and "stdio" is not a valid DTYP menu
        // choice for any other record type — so C rejects a DTYP="stdio" on
        // e.g. a stringin at db-LOAD time (illegal choice); were such a record
        // to reach init, its own init_record would raise S_dev_noDSET. base-rs
        // has no per-(recordType, DTYP) menu validation and so cannot replicate
        // the load-time rejection; the closest available enforcement is to gate
        // the record type here and Err, which `wire_device_to_record` turns
        // into a record INVALID alarm. Not a literal mirror of C's load-time
        // rejection, but the record is visibly unusable either way.
        let rt = record.record_type();
        if !matches!(rt, "lso" | "printf" | "stringout") {
            return Err(CaError::InvalidValue(format!(
                "DTYP='stdio': unsupported record type '{rt}' (use lso, printf, or stringout)"
            )));
        }
        // An OUT naming no known stream leaves `stream = None`. C `add_xxx`
        // returns -1 in that case, but iocInit's `doResolveLinks` discards the
        // return value (no `recGblRecordError`, no alarm) — so C is silent and
        // the record simply writes nothing. Match that: no diagnostic, no alarm.
        Ok(DeviceInitOutcome::Live)
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        // C `write_xxx`: `if (pstream) pstream->print("%s\n", prec->val)`.
        let Some(stream) = self.stream else {
            return Ok(());
        };
        if let Some(line) = val_string(record) {
            stream.print_line(&line);
        }
        Ok(())
    }
}

/// The record's `VAL` as the string `write()` prints. The three stdio
/// records carry VAL in two shapes: `stringout` as an `EpicsValue::String`,
/// `lso` and `printf` as an `EpicsValue::CharArray` (UTF-8 bytes). For a char
/// array, decode up to the first NUL — matching C `printf("%s\n", prec->val)`
/// reading the NUL-terminated `char *` VAL. Any other VAL shape yields `None`
/// (the record-type gate already restricts this to the three string records).
fn val_string(record: &dyn Record) -> Option<String> {
    match record.get_field("VAL")? {
        EpicsValue::String(s) => Some(s.as_str_lossy().into_owned()),
        EpicsValue::CharArray(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::records::lso::LsoRecord;
    use crate::server::records::stringin::StringinRecord;
    use crate::server::records::stringout::StringoutRecord;

    #[test]
    fn from_name_resolves_the_three_streams() {
        assert_eq!(StdioStream::from_name("stdout"), Some(StdioStream::Stdout));
        assert_eq!(StdioStream::from_name("stderr"), Some(StdioStream::Stderr));
        assert_eq!(StdioStream::from_name("errlog"), Some(StdioStream::Errlog));
        assert_eq!(StdioStream::from_name("bogus"), None);
        assert_eq!(StdioStream::from_name(""), None);
    }

    #[test]
    fn new_strips_inst_io_at_and_resolves_stream() {
        assert_eq!(
            StdioDeviceSupport::new("@stdout").stream,
            Some(StdioStream::Stdout)
        );
        assert_eq!(
            StdioDeviceSupport::new("stderr").stream,
            Some(StdioStream::Stderr)
        );
        assert_eq!(
            StdioDeviceSupport::new("@errlog").stream,
            Some(StdioStream::Errlog)
        );
        assert_eq!(StdioDeviceSupport::new("@nope").stream, None);
    }

    /// C registers no `(stringin, stdio)` dset; the record-type gate Errs so
    /// `wire_device_to_record` flags the record INVALID, matching "no device
    /// support for this DTYP".
    #[test]
    fn init_rejects_non_stdio_record_type() {
        let mut dev = StdioDeviceSupport::new("@stdout");
        let mut rec = StringinRecord::new("");
        assert!(dev.init(&mut rec).is_err());
    }

    #[test]
    fn init_accepts_stringout() {
        let mut dev = StdioDeviceSupport::new("@stdout");
        let mut rec = StringoutRecord::new("hello");
        assert!(dev.init(&mut rec).is_ok());
    }

    /// C `write_xxx` with `dpvt == NULL` (unknown stream) prints nothing and
    /// returns 0 — no alarm. Here `write` is a clean no-op.
    #[test]
    fn write_is_noop_on_unresolved_stream() {
        let mut dev = StdioDeviceSupport::new("@bogus");
        let mut rec = StringoutRecord::new("ignored");
        assert!(dev.write(&mut rec).is_ok());
    }

    /// A resolved stream prints VAL; assert the call path runs cleanly (the
    /// printed bytes go to the process stream — the `errlog` stream is
    /// asserted end-to-end via tracing capture in `tests/stdio_device.rs`).
    #[test]
    fn write_to_resolved_stream_succeeds() {
        let mut dev = StdioDeviceSupport::new("@stderr");
        let mut rec = StringoutRecord::new("hello stdio");
        assert!(dev.write(&mut rec).is_ok());
    }

    /// VAL extraction must read BOTH `EpicsValue::String` (`stringout`) and
    /// `EpicsValue::CharArray` (`lso`/`printf`) — a String-only match prints
    /// nothing for the two char-array records (C `print("%s\n", prec->val)`
    /// is the same for all three).
    #[test]
    fn val_string_reads_string_and_char_array_vals() {
        let so = StringoutRecord::new("from stringout");
        assert_eq!(val_string(&so).as_deref(), Some("from stringout"));

        let lso = LsoRecord::new("from lso");
        assert_eq!(val_string(&lso).as_deref(), Some("from lso"));
    }
}
